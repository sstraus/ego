use std::process::Command;

// Catches: a default credential deny masks the effective change in an exact user allow/revoke receipt.
#[test]
fn credential_preset_override_and_revoke_report_the_changed_read_authority() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join(".env"), "EXAMPLE=fixture\n").unwrap();
    let run = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_ego"))
            .env_clear()
            .env("HOME", home.path())
            .env("EGO_HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .current_dir(workspace.path())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let explain = || {
        let text = run(&[
            "rules",
            "explain",
            "read",
            r#"{"files":[{"path":".env"}]}"#,
            "--json",
        ]);
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["outcome"].clone()
    };
    assert_eq!(explain(), "denied");
    let allowed = run(&["rules", "allow", "read(.env)"]);
    assert_eq!(explain(), "allowed");
    let fields: Vec<_> = allowed.trim_end().split('\t').collect();
    assert_eq!(fields[1], "effective_changed", "{allowed}");
    let revoked = run(&["rules", "revoke", fields[3]]);
    assert_eq!(explain(), "denied");
    assert_eq!(
        revoked.trim_end().split('\t').nth(1),
        Some("effective_changed"),
        "{revoked}"
    );
}
