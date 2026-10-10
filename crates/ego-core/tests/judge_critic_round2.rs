use ego_core::config::{Mode, ModelRef};
use ego_core::permit::judge::{JudgeConfig, JudgeInput};
use ego_core::permit::{
    CanonPath, Classification, Confinement, EnvPolicy, ExecCaps, Op, PolicyEngine, PreparedCall,
    RunConfinement, SpawnSpec,
};

fn disclosed(argv: &[&str]) -> String {
    let call = PreparedCall::new(
        "exec",
        Op::Spawn {
            spec: SpawnSpec::new(
                argv.iter().map(|word| (*word).into()).collect(),
                Some(CanonPath::new("/ws")),
                None,
                EnvPolicy::Inherited,
            )
            .unwrap(),
        },
    );
    let policy = PolicyEngine::new(CanonPath::new("/ws"), Mode::Auto, Default::default());
    let Classification::Ask(pending) = policy.classify(call.clone()) else {
        panic!("curl must reach the advisory boundary");
    };
    let request = JudgeInput::new(
        &call,
        &RunConfinement::new(Confinement::Workspace, false, CanonPath::new("/ws")).unwrap(),
        ExecCaps::empty(),
        pending.request().why(),
    )
    .request(&JudgeConfig {
        identity: ModelRef("openrouter/example/judge".into()),
        request_model: "example/judge".into(),
        max_output_tokens: Some(512),
    });
    serde_json::to_string(&request.messages).unwrap()
}

// Catches: curl -suUSER:PASS bypasses the attached -u credential redaction.
#[test]
fn clustered_curl_user_password_must_not_be_disclosed_to_judge() {
    for argv in [
        vec![
            "curl",
            "-sualice:private-password",
            "https://api.example.com",
        ],
        vec![
            "bash",
            "-c",
            "curl -sualice:private-password https://api.example.com",
        ],
    ] {
        let wire = disclosed(&argv);
        assert!(
            !wire.contains("private-password"),
            "cross-provider wire contains clustered curl user credential"
        );
    }
}

// Catches: retaining Slack webhook URL paths releases the bearer credential to another provider.
#[test]
fn webhook_url_path_credential_must_not_be_disclosed_to_judge() {
    for argv in [
        vec![
            "curl",
            "https://hooks.slack.com/services/T000/B000/private-webhook-token",
        ],
        vec![
            "bash",
            "-c",
            "curl 'https://hooks.slack.com/services/T000/B000/private-webhook-token'",
        ],
    ] {
        let wire = disclosed(&argv);
        assert!(
            !wire.contains("private-webhook-token"),
            "cross-provider wire contains webhook URL bearer credential"
        );
        assert!(wire.contains("hooks.slack.com"));
    }
}
