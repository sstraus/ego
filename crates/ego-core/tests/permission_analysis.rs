use std::ffi::OsString;

use ego_core::permit::analysis::{RiskClass, RiskReason, assess};
use ego_core::permit::{
    CanonPath, Change, EnvName, EnvPolicy, ExplicitEnvBinding, Op, PreparedCall, SpawnSpec,
};

fn spawn(args: &[&str], value: &str) -> PreparedCall {
    PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                args.iter().map(OsString::from).collect(),
                Some(CanonPath::new("/ws")),
                None,
                EnvPolicy::exact(vec![
                    ExplicitEnvBinding::from_native(EnvName::new("TOKEN").unwrap(), value.into())
                        .unwrap(),
                ])
                .unwrap(),
            )
            .unwrap(),
        },
    )
}

#[test]
fn authority_fingerprint_binds_redacted_environment_values() {
    let first = spawn(&["tool", "a"], "secret-one");
    let second = spawn(&["tool", "a"], "secret-two");
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
    assert_ne!(
        first.authority_fingerprint(),
        second.authority_fingerprint()
    );
    assert_eq!(
        first.authority_fingerprint(),
        first.clone().authority_fingerprint()
    );
}

#[test]
fn authority_fingerprint_preserves_empty_ordered_and_native_arguments() {
    let original = spawn(&["tool", "a", "b"], "env").authority_fingerprint();
    for args in [
        &["tool", "ab"][..],
        &["tool", "b", "a"],
        &["tool", "a", "b", ""],
    ] {
        assert_ne!(original, spawn(args, "env").authority_fingerprint());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let native = PreparedCall::new(
            "exec",
            Op::Spawn {
                spec: SpawnSpec::new(
                    vec!["tool".into(), OsString::from_vec(vec![255])],
                    Some(CanonPath::new("/ws")),
                    None,
                    EnvPolicy::Inherited,
                )
                .unwrap(),
            },
        );
        assert_ne!(
            native.authority_fingerprint(),
            spawn(&["tool", "�"], "env").authority_fingerprint()
        );
    }
}

#[test]
fn risk_classes_and_reasons_do_not_trust_process_names() {
    let read = PreparedCall::new(
        "read",
        Op::Read {
            path: CanonPath::new("/ws/file"),
        },
    );
    assert_eq!(assess(&read).class, RiskClass::Low);
    assert_eq!(assess(&read).reasons, vec![RiskReason::ReadOnly]);
    let write = read.with_op(Op::Write {
        path: CanonPath::new("/ws/file"),
        change: Change::Whole(b"text".to_vec()),
    });
    assert_eq!(assess(&write).class, RiskClass::Bounded);
    assert_eq!(
        assess(&write).reasons,
        vec![RiskReason::ReadOnly, RiskReason::ScopedWrite]
    );
    for args in [
        &["git", "status"][..],
        &["sh", "-c", "echo ok"],
        &["python", "-m", "package"],
        &["npm", "test"],
    ] {
        let risk = assess(&spawn(args, "env"));
        assert_eq!(risk.class, RiskClass::Unknown);
        assert!(risk.reasons.contains(&RiskReason::IdentityIncomplete));
        assert_eq!(
            risk.reasons,
            vec![
                RiskReason::ProcessEffectsUnknown,
                RiskReason::IdentityIncomplete
            ]
        );
    }
    assert!(RiskClass::High > RiskClass::Unknown);
    assert!(RiskClass::Unknown > RiskClass::Bounded);
    assert!(RiskClass::Bounded > RiskClass::Low);
}

#[test]
fn each_policy_backend_trust_and_judge_dimension_invalidates_analysis() {
    use ego_core::config::{Mode, ModelRef, Ruleset, Trust};
    use ego_core::permit::analysis::cache_key;
    use ego_core::permit::judge::JudgeConfig;
    use ego_core::permit::{BackendIdentity, ExecCaps, ExecObservation};
    use ego_core::turn::GenerationPolicyCapture;
    let policy =
        GenerationPolicyCapture::new(CanonPath::new("/ws"), Mode::Auto, Ruleset::default(), None)
            .unwrap()
            .snapshot()
            .digest();
    let observation = ExecObservation::new(
        BackendIdentity::new("backend", "1"),
        ExecCaps::READ_SCOPED,
        "11".repeat(32).try_into().unwrap(),
    );
    let judge = JudgeConfig {
        identity: ModelRef("provider/judge".into()),
        request_model: "wire-model".into(),
        max_output_tokens: Some(100),
    };
    let call = spawn(&["tool", "arg"], "env");
    let original = cache_key(&call, &policy, &observation, Trust::Trusted, &judge);
    let Op::Spawn { spec } = &call.ops()[0] else {
        unreachable!()
    };
    let changed_cwd = PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                spec.argv().to_vec(),
                Some(CanonPath::new("/ws/other")),
                spec.timeout(),
                spec.env_policy().clone(),
            )
            .unwrap(),
        },
    );
    assert_ne!(
        original,
        cache_key(&changed_cwd, &policy, &observation, Trust::Trusted, &judge)
    );
    for changed in [
        spawn(&["tool", "other"], "env"),
        spawn(&["tool", "arg"], "changed"),
    ] {
        assert_ne!(
            original,
            cache_key(&changed, &policy, &observation, Trust::Trusted, &judge)
        );
    }
    assert_ne!(
        original,
        cache_key(&call, &policy, &observation, Trust::Untrusted, &judge)
    );
    for changed in [
        ExecObservation::new(
            BackendIdentity::new("other", "1"),
            ExecCaps::READ_SCOPED,
            "11".repeat(32).try_into().unwrap(),
        ),
        ExecObservation::new(
            BackendIdentity::new("backend", "2"),
            ExecCaps::READ_SCOPED,
            "11".repeat(32).try_into().unwrap(),
        ),
        ExecObservation::new(
            BackendIdentity::new("backend", "1"),
            ExecCaps::WRITE_SCOPED,
            "11".repeat(32).try_into().unwrap(),
        ),
        ExecObservation::new(
            BackendIdentity::new("backend", "1"),
            ExecCaps::READ_SCOPED,
            "22".repeat(32).try_into().unwrap(),
        ),
    ] {
        assert_ne!(
            original,
            cache_key(&call, &policy, &changed, Trust::Trusted, &judge)
        );
    }
    for changed in [
        JudgeConfig {
            identity: ModelRef("provider/new-judge".into()),
            ..judge.clone()
        },
        JudgeConfig {
            request_model: "new-wire-model".into(),
            ..judge.clone()
        },
        JudgeConfig {
            max_output_tokens: Some(200),
            ..judge.clone()
        },
    ] {
        assert_ne!(
            original,
            cache_key(&call, &policy, &observation, Trust::Trusted, &changed)
        );
    }
    for (workspace, mode) in [("/other", Mode::Auto), ("/ws", Mode::Plan)] {
        let changed =
            GenerationPolicyCapture::new(CanonPath::new(workspace), mode, Ruleset::default(), None)
                .unwrap()
                .snapshot()
                .digest();
        assert_ne!(
            original,
            cache_key(&call, &changed, &observation, Trust::Trusted, &judge)
        );
    }
}

#[test]
fn incomplete_identity_allows_only_reusable_escalation_or_denial() {
    use ego_core::permit::analysis::{AnalysisOpinion, complete_identity};
    let call = spawn(&["sh", "-c", "source secret"], "secret");
    assert!(!complete_identity(&call));
    assert!(!AnalysisOpinion::Allow.reusable(false));
    assert!(AnalysisOpinion::Escalate.reusable(false));
    assert!(AnalysisOpinion::Deny.reusable(false));
    assert!(AnalysisOpinion::Allow.reusable(true));
    assert_eq!(assess(&call).class, RiskClass::Unknown);
}

#[test]
fn invalid_judge_output_has_no_cacheable_verdict() {
    use ego_core::generator::{port::AiEvent, types::StopReason};
    use ego_core::permit::judge::JudgeDecoder;
    for text in [
        "{",
        "not JSON",
        r#"{"verdict":"allow","reason":"ok","scope":"all"}"#,
    ] {
        let mut decoder = JudgeDecoder::new();
        decoder.observe(&AiEvent::TextDelta { text: text.into() });
        decoder.observe(&AiEvent::Stop(StopReason::Stop));
        assert!(decoder.finish_valid().is_err());
    }
    let mut decoder = JudgeDecoder::new();
    decoder.observe(&AiEvent::TextDelta {
        text: r#"{"verdict":"escalate","reason":"unknown"}"#.into(),
    });
    decoder.observe(&AiEvent::Stop(StopReason::Stop));
    assert!(decoder.finish_valid().is_ok());
}

#[test]
fn inline_source_and_environment_values_never_enter_the_judge_projection() {
    use ego_core::config::{Mode, ModelRef};
    use ego_core::permit::judge::{JudgeConfig, JudgeInput};
    use ego_core::permit::{Confinement, Escalation, ExecCaps, OpName, RunConfinement};
    let call = spawn(
        &["sh", "-c", "echo unique-inline-source-secret"],
        "unique-environment-secret",
    );
    let config = JudgeConfig {
        identity: ModelRef("provider/model".into()),
        request_model: "model".into(),
        max_output_tokens: Some(100),
    };
    let confinement =
        RunConfinement::new(Confinement::Workspace, true, CanonPath::new("/ws")).unwrap();
    let request = JudgeInput::new(
        &call,
        &confinement,
        ExecCaps::READ_SCOPED,
        &Escalation::ModeAsks {
            mode: Mode::Auto,
            op: OpName::Spawn,
            path: CanonPath::new("/ws"),
        },
    )
    .request(&config);
    let encoded = serde_json::to_string(&request).unwrap();
    assert!(!encoded.contains("unique-inline-source-secret"));
    assert!(!encoded.contains("unique-environment-secret"));
    assert!(encoded.contains("sha256"));
    assert!(encoded.contains("unknown"));
}
