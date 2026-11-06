use ego_core::permit::shell::observe;

// Catches: recognized curl credentials and unknown option operands cross the disclosure boundary.
#[test]
fn curl_credential_and_unknown_option_operands_are_private_in_native_and_shell_actions() {
    for flag in [
        "--oauth2-bearer",
        "-u",
        "--user",
        "-H",
        "--cookie",
        "-E",
        "--cert",
        "--key",
        "--pass",
        "--proxy-user",
        "--future-credential",
    ] {
        for argv in [
            vec![
                "curl".into(),
                flag.into(),
                "private-value".into(),
                "https://api.example.com".into(),
            ],
            vec![
                "bash".into(),
                "-c".into(),
                format!("curl {flag} private-value https://api.example.com"),
            ],
            vec![
                "bash".into(),
                "-c".into(),
                format!("curl {flag} 'private-value' https://api.example.com"),
            ],
            vec![
                "bash".into(),
                "-c".into(),
                format!("curl '{flag}' private-value https://api.example.com"),
            ],
        ] {
            let skeleton = observe(&argv).skeleton;
            assert!(!skeleton.contains("private-value"), "{flag}: {skeleton}");
            assert!(skeleton.contains("api.example.com"), "{flag}: {skeleton}");
        }
    }
}

// Catches: attached short-option credentials and equals-form options bypass operand redaction.
#[test]
fn attached_curl_credentials_are_private() {
    for option in [
        "-uprivate-value",
        "-HAuthorization:private-value",
        "-Eprivate-value",
        "-bprivate-value",
        "--oauth2-bearer=private-value",
        "--future-credential=private-value",
    ] {
        for argv in [
            vec!["curl".into(), option.into()],
            vec!["bash".into(), "-c".into(), format!("curl {option}")],
        ] {
            assert!(
                !observe(&argv).skeleton.contains("private-value"),
                "{option}"
            );
        }
    }
}
