use super::*;

// Catches: exec_paths cannot be managed, profile edits overwrite base, or invalid paths publish.
#[test]
fn exec_paths_round_trip_and_unset_preserve_target_and_reject_relative_paths() {
    let fixture = Fixture::new();
    fixture.write_config("# preserve\nmodel='ollama/qwen3'\n");
    let assignment = format!("exec_paths=['{}']", fixture.workspace.path().display());
    let set = fixture.run(&["config", "set", &assignment]);
    assert!(set.status.success(), "{}", error(&set));
    let listed = fixture.run(&["config", "ls", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(
        value["values"]["exec_paths"],
        serde_json::json!([fixture.workspace.path()])
    );
    assert!(
        fixture
            .run(&["config", "set", "--profile", "ci", "exec_paths=[]"])
            .status
            .success()
    );
    let before = std::fs::read(fixture.config_path()).unwrap();
    assert!(
        !fixture
            .run(&["config", "set", "exec_paths=['relative']"])
            .status
            .success()
    );
    assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);
    assert!(
        fixture
            .run(&["config", "unset", "--profile", "ci", "exec_paths"])
            .status
            .success()
    );
    assert!(
        fixture
            .run(&["config", "get", "exec_paths"])
            .status
            .success()
    );
    assert!(
        fixture
            .run(&["config", "unset", "exec_paths"])
            .status
            .success()
    );
    let stored = std::fs::read_to_string(fixture.config_path()).unwrap();
    assert!(stored.contains("# preserve"));
    assert!(stored.contains("ollama/qwen3"));
    assert!(!stored.contains("exec_paths"));
}

// Catches: managed headers bypass the case-insensitive forbidden override rule or publish on refusal.
#[test]
fn auth_headers_refuse_forbidden_names_without_publishing() {
    let fixture = Fixture::new();
    fixture.write_config("model='ollama/qwen3'\n");
    let before = std::fs::read(fixture.config_path()).unwrap();
    for name in [
        "authorization",
        "x-api-key",
        "content-type",
        "accept",
        "openai-beta",
        "chatgpt-account-id",
        "Authorization",
        "X-API-KEY",
    ] {
        let assignment = format!("auth.openai.headers={{'{name}'='SECRET-HEADER'}}");
        let output = fixture.run(&["config", "set", &assignment]);
        assert_usage_failure(&output, "invalid TOML value or type");
        assert!(!error(&output).contains("SECRET-HEADER"));
        assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);
    }
}

// Catches: auth secrets leak in ls/get/receipts, profile auth is accepted, or unset erases siblings.
#[test]
fn auth_managed_values_are_base_only_and_redacted_in_output() {
    let fixture = Fixture::new();
    for assignment in [
        "auth.openai.headers={'X-Identity'='SECRET-HEADER'}",
        "auth.openai.credential_command='SECRET-COMMAND'",
        "auth.other.headers={'X-Identity'='OTHER-SECRET'}",
        "auth.provider.with.dots.headers={'X-Identity'='DOTTED-SECRET'}",
    ] {
        let set = fixture.run(&["config", "set", assignment]);
        assert!(set.status.success(), "{}", error(&set));
        assert!(!text(&set).contains("SECRET"));
        let before = std::fs::read(fixture.config_path()).unwrap();
        assert_usage_failure(
            &fixture.run(&["config", "set", "--profile", "ci", assignment]),
            "not available",
        );
        assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);
    }
    let stored = std::fs::read_to_string(fixture.config_path()).unwrap();
    assert!(stored.contains("SECRET-HEADER") && stored.contains("SECRET-COMMAND"));
    for args in [
        vec!["config", "ls"],
        vec!["config", "ls", "--json"],
        vec!["config", "get", "auth.openai.headers"],
        vec!["config", "get", "--json", "auth.openai.credential_command"],
    ] {
        let output = fixture.run(&args);
        assert!(output.status.success(), "{}", error(&output));
        assert!(!text(&output).contains("SECRET"));
        assert!(!error(&output).contains("SECRET"));
        assert!(text(&output).contains("<redacted>"));
    }
    for key in ["auth.openai.headers", "auth.openai.credential_command"] {
        assert_usage_failure(
            &fixture.run(&["config", "unset", "--profile", "ci", key]),
            "not available",
        );
        assert!(fixture.run(&["config", "unset", key]).status.success());
    }
    let stored = std::fs::read_to_string(fixture.config_path()).unwrap();
    assert!(!stored.contains("SECRET-COMMAND") && !stored.contains("SECRET-HEADER"));
    assert!(stored.contains("OTHER-SECRET"));
}

// Catches: absent/default headers block unsets, empty maps lose persistence, or bad types are published.
#[test]
fn auth_empty_headers_and_command_only_tables_preserve_absence_and_types() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .run(&[
                "config",
                "set",
                "auth.openai.credential_command='SECRET-COMMAND'"
            ])
            .status
            .success()
    );
    let listed = fixture.run(&["config", "ls", "--json"]);
    let value: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert!(value["values"].get("auth.openai.headers").is_none());
    assert!(
        fixture
            .run(&["config", "unset", "auth.openai.headers"])
            .status
            .success()
    );
    assert!(
        fixture
            .run(&["config", "set", "auth.openai.headers={}"])
            .status
            .success()
    );
    let before = std::fs::read(fixture.config_path()).unwrap();
    for assignment in [
        "auth.openai.headers='SECRET-WRONG-TYPE'",
        "auth.openai.headers={identity=123}",
        "auth.openai.credential_command=123",
    ] {
        let output = fixture.run(&["config", "set", assignment]);
        assert_usage_failure(&output, "invalid TOML value or type");
        assert!(!error(&output).contains("SECRET"));
        assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);
    }
    assert!(
        fixture
            .run(&["config", "unset", "auth.openai.headers"])
            .status
            .success()
    );
    assert!(
        fixture
            .run(&["config", "unset", "auth.openai.headers"])
            .status
            .success()
    );
    let stored = std::fs::read_to_string(fixture.config_path()).unwrap();
    assert!(stored.contains("SECRET-COMMAND"));
    assert!(!stored.contains("headers"));
}
