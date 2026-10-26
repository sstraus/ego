mod support;
use ego_core::permit::judge::{JudgeRecentCall, JudgeTaskContext};

fn request_text(argv: &[String]) -> String {
    request_with_context(argv, &JudgeTaskContext::default())
}

fn request_with_context(argv: &[String], context: &JudgeTaskContext) -> String {
    let request = support::judge_request(&support::judge_call(argv), context);
    serde_json::to_string(&request.messages).unwrap()
}

// Catches: URL redaction erases destination/shape or leaks path/userinfo/query/fragment text.
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
            for required in ["https", "upload.example.com", "8443", "path-segments=1"] {
                assert!(skeleton.contains(required), "{required}: {skeleton}");
            }
            for secret in [
                "collect",
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

// Catches: recursively redacting a generated string marker erases its sensitivity class.
#[test]
fn generated_path_classification_survives_cross_provider_request() {
    let argv = vec![
        "bash".into(),
        "-c".into(),
        "cat '/private/workspace/.env'".into(),
    ];
    assert!(request_text(&argv).contains("class=absolute:sensitive"));
    let structure = ego_core::permit::shell::observe(&argv).skeleton;
    let context = JudgeTaskContext {
        recent_calls: vec![JudgeRecentCall {
            tool: "exec".into(),
            structure,
            ok: Some(true),
        }],
        ..Default::default()
    };
    let text = request_with_context(&["pwd".into()], &context);
    assert!(text.contains("class=absolute:sensitive"), "{text}");
}

// Catches: scheme-less URL query and fragment credentials escape URL projection.
#[test]
fn schemeless_url_credentials_do_not_cross_provider_boundary() {
    for url in [
        "upload.example.com/collect?token=private-query#private-fragment",
        "//upload.example.com/collect?token=private-query#private-fragment",
    ] {
        let argv = vec!["curl".into(), url.into()];
        let text = request_text(&argv);
        assert!(!text.contains("private-query"), "{text}");
        assert!(!text.contains("private-fragment"), "{text}");
        assert!(text.contains("upload.example.com"), "{text}");
    }
}

// Catches: attached mysql passwords are disclosed, while generic -p handling hides ssh ports.
#[test]
fn password_option_projection_is_command_aware() {
    for argv in [
        vec!["mysql".into(), "-pprivate-password".into()],
        vec![
            "bash".into(),
            "-c".into(),
            "mysql -pprivate-password".into(),
        ],
    ] {
        assert!(!request_text(&argv).contains("private-password"));
    }
    let text = request_text(&["ssh".into(), "-p2222".into(), "host".into()]);
    assert!(text.contains("-p2222"), "{text}");
}

// Catches: slash/dot alphabet exclusions let credential-shaped entropy reach another provider.
#[test]
fn high_entropy_atoms_with_slashes_and_dots_are_private() {
    for secret in [
        "aB3dE5fG7hI9jK1mN2oP4qR6sT8uV/wX",
        "aB3dE5fG7hI9jK1mN2oP4qR6sT8uV.wX",
    ] {
        assert!(!request_text(&["echo".into(), secret.into()]).contains(secret));
    }
}

// Catches: substring matching redacts ordinary disk-1 as an OpenAI credential.
#[test]
fn ordinary_disk_name_is_not_an_openai_secret() {
    let text = request_text(&["ls".into(), "disk-1".into()]);
    assert!(text.contains("disk-1"), "{text}");
    assert!(!text.contains("secret:openai"), "{text}");
}

// Catches: fixing the helper without applying it to task/reason/root disclosure.
#[test]
fn authorization_credentials_in_task_context_do_not_reach_judge() {
    let context = JudgeTaskContext {
        task: "Inspect Authorization: Bearer task.secret== safely".into(),
        agent_reason: "Check authorization: Basic reason-secret= safely".into(),
        roots: vec!["Authorization: Bearer root.secret- safely".into()],
        ..Default::default()
    };
    let text = request_with_context(&["pwd".into()], &context);
    for secret in ["task.secret==", "reason-secret=", "root.secret-"] {
        assert!(!text.contains(secret), "{text}");
    }
    assert!(text.contains("Inspect"), "{text}");
    assert!(text.contains("safely"), "{text}");
}
