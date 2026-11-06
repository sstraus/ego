use crate::support;

fn disclosed(argv: &[&str]) -> String {
    let argv = argv
        .iter()
        .map(|word| (*word).to_owned())
        .collect::<Vec<_>>();
    let request = support::judge_request(&support::judge_call(&argv), &Default::default());
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

// Catches: HTTP-only projection releases other URL schemes' userinfo and bearer paths.
#[test]
fn non_http_url_credentials_must_not_be_disclosed_to_judge() {
    let mut leaks = Vec::new();
    for url in [
        "ftp://alice:private-password@files.example.com/private-path-token",
        "git+ssh://alice:private-password@git.example.com/private-path-token",
    ] {
        for argv in [
            vec!["curl".into(), url.into()],
            vec!["bash".into(), "-c".into(), format!("curl {url}")],
            vec!["bash".into(), "-c".into(), format!("curl '{url}'")],
        ] {
            let borrowed = argv.iter().map(String::as_str).collect::<Vec<_>>();
            let wire = disclosed(&borrowed);
            if wire.contains("private-password") || wire.contains("private-path-token") {
                leaks.push(argv);
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "URL credentials disclosed to judge: {leaks:?}"
    );
}
