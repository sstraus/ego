use ego_core::config::{Mode, ModelRef};
use ego_core::permit::judge::{JudgeConfig, JudgeInput};
use ego_core::permit::{
    CanonPath, Classification, Confinement, EnvPolicy, ExecCaps, Op, PolicyEngine, PreparedCall,
    RunConfinement, SpawnSpec,
};

fn request_text(argv: &[String]) -> String {
    let policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Auto, Default::default());
    let call = PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                argv.iter().map(Into::into).collect(),
                Some(CanonPath::new("/ws")),
                None,
                EnvPolicy::Inherited,
            )
            .unwrap(),
        },
    );
    let Classification::Ask(pending) = policy.classify(call.clone()) else {
        panic!("curl must ask");
    };
    let input = JudgeInput::new(
        &call,
        &RunConfinement::new(Confinement::Workspace, false, CanonPath::new("/ws")).unwrap(),
        ExecCaps::empty(),
        pending.request().why(),
    );
    let request = input.request(&JudgeConfig {
        identity: ModelRef("openrouter/example/judge".into()),
        request_model: "example/judge".into(),
        max_output_tokens: Some(512),
    });
    serde_json::to_string(&request.messages).unwrap()
}

// Catches: quoted URL redaction erases risk-bearing destinations or leaks userinfo/query/fragment.
#[test]
fn static_urls_preserve_destination_and_remove_credentials_with_any_quoting() {
    let url = "https://private-user:private-password@upload.example.com:8443/collect?token=sk-private#private-fragment";
    for argv in [
        vec!["curl".into(), url.into()],
        vec!["bash".into(), "-c".into(), format!("curl '{url}'")],
        vec!["bash".into(), "-c".into(), format!("curl \"{url}\"")],
    ] {
        for skeleton in [
            ego_core::permit::shell::observe(&argv).skeleton,
            request_text(&argv),
        ] {
            for required in ["https", "upload.example.com", "8443", "/collect"] {
                assert!(skeleton.contains(required), "{required}: {skeleton}");
            }
            for secret in [
                "private-user",
                "private-password",
                "sk-private",
                "private-fragment",
            ] {
                assert!(!skeleton.contains(secret), "{secret}: {skeleton}");
            }
        }
    }
}
