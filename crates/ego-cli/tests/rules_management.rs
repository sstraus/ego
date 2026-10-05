//! contract for regression case's final `ego rules` management surface.

use std::process::{Command, Output};

struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
            workspace: tempfile::tempdir().unwrap(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    fn run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ego"));
        command
            .env_clear()
            .env("HOME", self.home.path())
            .env("EGO_HOME", self.home.path())
            .env("PATH", "/usr/bin:/bin")
            .current_dir(self.workspace.path())
            .args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }
}

fn stdout(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("UTF-8 stdout")
}

fn user_rows(value: &serde_json::Value) -> Vec<&serde_json::Value> {
    value["rules"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["from"] == "user")
        .collect()
}

fn stderr(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("UTF-8 stderr")
}

#[test]
fn shallow_allow_is_persistent_and_has_the_exact_eight_field_receipt() {
    let fixture = Fixture::new();
    let result = fixture.run(&["rules", "allow", "read(src/**)"]);
    assert_eq!(result.status.code(), Some(0), "{}", stderr(&result));
    assert_eq!(stdout(&result).trim_end().split('\t').count(), 8);
    let fields: Vec<_> = stdout(&result).trim_end().split('\t').collect();
    assert_eq!(fields[0], "stored_changed");
    assert_eq!(fields[1], "effective_changed");
    assert_eq!(fields[2], "allow");
    assert!(fields[3].starts_with("r1:"));
    assert_eq!(fields[4], "read(src/**)");
    assert_eq!(fields[5], "user");
    assert_eq!(fields[6], "-");
    assert_eq!(fields[7], "published_durably");

    let listed = fixture.run(&["rules", "ls"]);
    assert_eq!(listed.status.code(), Some(0), "{}", stderr(&listed));
    assert!(stdout(&listed).contains("read(src/**)"));
    assert_eq!(fixture.run(&["rules", "ls"]).stdout, listed.stdout);
}

#[test]
fn ls_text_and_json_have_deterministic_effect_id_rule_from_also_rows() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .run(&["rules", "allow", "read(src/**)"])
            .status
            .code(),
        Some(0)
    );
    assert_eq!(
        fixture
            .run(&["rules", "deny", "exec(cargo *)"])
            .status
            .code(),
        Some(0)
    );
    let text = fixture.run(&["rules", "ls"]);
    assert_eq!(text.status.code(), Some(0));
    let text_rows: Vec<_> = stdout(&text)
        .lines()
        .filter(|row| row.split('\t').nth(3) == Some("user"))
        .collect();
    assert_eq!(text_rows.len(), 2);
    for row in &text_rows {
        assert_eq!(row.split('\t').count(), 5);
        assert!(row.split('\t').next().unwrap().starts_with("r1:"));
    }
    let json = fixture.run(&["rules", "ls", "--json"]);
    assert_eq!(json.status.code(), Some(0));
    let raw_json = stdout(&json);
    for (left, right) in [
        ("\"v\"", "\"scope\""),
        ("\"scope\"", "\"profile\""),
        ("\"profile\"", "\"rules\""),
    ] {
        assert!(raw_json.find(left).unwrap() < raw_json.find(right).unwrap());
    }
    let value: serde_json::Value = serde_json::from_str(raw_json).unwrap();
    assert_eq!(value["v"], 1);
    assert_eq!(value["scope"], "effective");
    assert!(value["profile"].is_null());
    let rows = user_rows(&value);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["effect"], "deny");
    assert_eq!(rows[0]["rule"], "exec(cargo *)");
    assert_eq!(rows[1]["effect"], "allow");
    assert_eq!(rows[1]["rule"], "read(src/**)");
    for row in rows {
        assert!(row.is_object());
        assert!(row["id"].as_str().unwrap().starts_with("r1:"));
        assert!(row["from"].is_string());
        assert!(row["also"].is_array());
    }
    for (left, right) in [
        ("\"id\"", "\"effect\""),
        ("\"effect\"", "\"rule\""),
        ("\"rule\"", "\"from\""),
        ("\"from\"", "\"also\""),
    ] {
        assert!(raw_json.find(left).unwrap() < raw_json.find(right).unwrap());
    }
}

#[test]
fn management_ignores_ego_profile_and_explicit_profile_isolated_and_persistent() {
    let fixture = Fixture::new();
    let base = fixture.run_env(
        &["rules", "allow", "read(src/**)"],
        &[("EGO_PROFILE", "ci")],
    );
    assert_eq!(base.status.code(), Some(0), "{}", stderr(&base));
    let profile = fixture.run(&["rules", "ls", "--profile", "ci", "--json"]);
    assert_eq!(profile.status.code(), Some(0));
    let profile_value: serde_json::Value = serde_json::from_str(stdout(&profile)).unwrap();
    assert_eq!(profile_value["profile"], "ci");
    assert!(user_rows(&profile_value).is_empty());
    let base_value: serde_json::Value =
        serde_json::from_str(stdout(&fixture.run(&["rules", "ls", "--json"]))).unwrap();
    assert_eq!(base_value["profile"], serde_json::Value::Null);
    assert_eq!(user_rows(&base_value).len(), 1);
    let profile_set = fixture.run(&["rules", "allow", "--profile", "ci", "write(src/**)"]);
    assert_eq!(profile_set.status.code(), Some(0));
    assert!(stdout(&fixture.run(&["rules", "ls", "--profile", "ci"])).contains("write(src/**)"));
    assert!(stdout(&fixture.run(&["rules", "ls"])).contains("read(src/**)"));
}

#[test]
fn allow_deny_are_idempotent_and_opposite_effect_is_a_conflict() {
    let fixture = Fixture::new();
    let first = fixture.run(&["rules", "allow", "read(src/**)"]);
    assert_eq!(first.status.code(), Some(0));
    let duplicate = fixture.run(&["rules", "allow", "read( src/** )"]);
    assert_eq!(duplicate.status.code(), Some(0));
    let duplicate_fields: Vec<_> = stdout(&duplicate).trim_end().split('\t').collect();
    assert_eq!(duplicate_fields.len(), 8);
    assert_eq!(duplicate_fields[0], "stored_unchanged");
    assert_eq!(duplicate_fields[1], "effective_unchanged");
    assert_eq!(duplicate_fields[2], "allow");
    assert_eq!(duplicate_fields[4], "read(src/**)");
    assert_eq!(duplicate_fields[6], "user");
    assert_eq!(duplicate_fields[7], "not_published");
    let conflict = fixture.run(&["rules", "deny", "read(src/**)"]);
    assert_eq!(conflict.status.code(), Some(1));
    assert!(stdout(&conflict).is_empty());
    assert!(stderr(&conflict).contains("read(src/**)"));
}

#[test]
fn revoke_existing_and_missing_are_persistent_noop_or_change_and_prune_permissions() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .run(&["rules", "allow", "read(src/**)"])
            .status
            .code(),
        Some(0)
    );
    let listed = fixture.run(&["rules", "ls", "--json"]);
    let value: serde_json::Value = serde_json::from_str(stdout(&listed)).unwrap();
    let id = user_rows(&value)[0]["id"].as_str().unwrap().to_owned();
    let revoked = fixture.run(&["rules", "revoke", &id]);
    assert_eq!(revoked.status.code(), Some(0));
    assert!(stdout(&revoked).starts_with("stored_changed\t"));
    let missing = fixture.run(&["rules", "revoke", &id]);
    assert_eq!(missing.status.code(), Some(0));
    assert!(stdout(&missing).starts_with("stored_unchanged\t"));
    assert!(
        !stdout(&fixture.run(&["rules", "ls"]))
            .lines()
            .any(|row| row.split('\t').nth(3) == Some("user"))
    );
    let future = fixture.run(&["rules", "revoke", "r2:000000000000"]);
    assert_eq!(future.status.code(), Some(2));
    assert!(stdout(&future).is_empty());
}

#[test]
fn remember_preview_is_nonpublishing_and_yes_persists_canonical_rule() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.workspace.path().join("src")).unwrap();
    std::fs::write(
        fixture.workspace.path().join("src/main.rs"),
        "fn main() {}\n",
    )
    .unwrap();
    let preview = fixture.run(&[
        "rules",
        "remember",
        "allow",
        "read",
        r#"{"path":"src/main.rs"}"#,
    ]);
    assert_eq!(preview.status.code(), Some(0));
    assert!(stdout(&preview).contains("DERIVED"));
    assert!(stdout(&preview).contains("DERIVED\t0\tallow\t"));
    assert!(
        stdout(&preview)
            .contains("RESULT\tstored_unchanged\teffective_unchanged\tuser\tnot_published\n")
    );
    assert!(
        !stdout(&fixture.run(&["rules", "ls"]))
            .lines()
            .any(|row| row.split('\t').nth(3) == Some("user"))
    );
    let accepted = fixture.run(&[
        "rules",
        "remember",
        "--yes",
        "allow",
        "read",
        r#"{"path":"src/main.rs"}"#,
    ]);
    assert_eq!(accepted.status.code(), Some(0), "{}", stderr(&accepted));
    assert!(
        stdout(&accepted)
            .contains("RESULT\tstored_changed\teffective_changed\tuser\tpublished_durably\n")
    );
    assert!(stdout(&fixture.run(&["rules", "ls"])).contains("read("));
    let refused = fixture.run(&[
        "rules",
        "remember",
        "--yes",
        "allow",
        "read_tool_result",
            r#"{"reference":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef:0","offset":0}"#,
    ]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(stdout(&refused).is_empty());
}

#[test]
fn explain_is_dry_and_versioned_with_redacted_operations_and_no_execution() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.workspace.path().join("src")).unwrap();
    std::fs::write(
        fixture.workspace.path().join("src/main.rs"),
        "fn main() {}\n",
    )
    .unwrap();
    assert_eq!(
        fixture
            .run(&["rules", "allow", "read(src/**)"])
            .status
            .code(),
        Some(0)
    );
    let explained = fixture.run(&[
        "rules",
        "explain",
        "read",
        r#"{"path":"src/main.rs"}"#,
        "--json",
    ]);
    assert_eq!(explained.status.code(), Some(0), "{}", stderr(&explained));
    let value: serde_json::Value = serde_json::from_str(stdout(&explained)).unwrap();
    assert_eq!(value["v"], 1);
    assert!(value["operations"].is_array());
    assert!(matches!(
        value["outcome"].as_str(),
        Some("allowed" | "denied" | "uncovered")
    ));
    assert_eq!(value["settlement"], "not_run");
    assert_eq!(value["would_execute"], false);
    assert_eq!(value["scope"], "effective");
    assert_eq!(value["profile"], serde_json::Value::Null);
    assert_eq!(value["tool"], "read");
    assert_eq!(value["operations"][0]["index"], 0);
    assert_eq!(value["operations"][0]["kind"], "read");
    assert!(value["operations"][0]["subject"].is_string());
    assert!(value["authorities"].is_array());
    for authority in value["authorities"].as_array().unwrap() {
        assert!(authority.is_object());
        assert!(authority["from"].is_string());
        assert!(authority["also"].is_array());
    }
    assert_eq!(value["outcome"], "allowed");
    assert!(value["uncovered"].is_array());
    assert!(!stdout(&explained).contains(fixture.workspace.path().to_str().unwrap()));
}

#[test]
fn rules_usage_and_secret_failures_are_exit_two_without_stdout_or_raw_values() {
    let fixture = Fixture::new();
    for args in [
        &["rules", "allow"] as &[&str],
        &["rules", "ls", "unexpected"] as &[&str],
        &["rules", "allow", "read(src/**)", "extra"] as &[&str],
        &["rules", "allow", "--json", "read(src/**)"],
        &["rules", "deny", "--json", "exec(cargo *)"],
        &["rules", "revoke", "--json", "r1:000000000000"],
        &[
            "rules",
            "remember",
            "allow",
            "read",
            r#"{"path":"src/main.rs"}"#,
            "extra",
        ],
        &[
            "rules",
            "explain",
            "read",
            r#"{"path":"src/main.rs"}"#,
            "extra",
        ],
        &["rules", "allow", "SECRET-DO-NOT-LEAK"],
        &[
            "rules",
            "explain",
            "unknown_tool",
            r#"{"secret":"NO-LEAK"}"#,
        ],
        &[
            "rules",
            "remember",
            "--yes",
            "unknown_tool",
            r#"{"secret":"NO-LEAK"}"#,
        ],
        &[
            "rules",
            "remember",
            "--yes",
            "allow",
            "unknown_tool",
            r#"{"secret":"NO-LEAK"}"#,
        ],
    ] {
        let output = fixture.run(args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(!stderr(&output).contains("NO-LEAK"));
    }
}

// Catches persisted user re-allow not reaching classification, or revoke leaving it enabled.
#[test]
fn credential_defaults_persisted_user_allow_and_revoke_change_read_explanation() {
    let fixture = Fixture::new();
    std::fs::write(fixture.workspace.path().join(".env"), "EXAMPLE=public\n").unwrap();
    let explain = || {
        let result = fixture.run(&["rules", "explain", "read", r#"{"path":".env"}"#, "--json"]);
        assert_eq!(result.status.code(), Some(0), "{}", stderr(&result));
        serde_json::from_str::<serde_json::Value>(stdout(&result)).unwrap()
    };
    let denied = explain();
    assert_eq!(denied["outcome"], "denied");
    assert_eq!(denied["authorities"][0]["from"], "default");
    let allowed = fixture.run(&["rules", "allow", "read(.env)"]);
    assert_eq!(allowed.status.code(), Some(0), "{}", stderr(&allowed));
    let id = stdout(&allowed).split('\t').nth(3).unwrap().to_owned();
    let granted = explain();
    assert_eq!(granted["outcome"], "allowed");
    assert_eq!(granted["authorities"][0]["from"], "user");
    let revoked = fixture.run(&["rules", "revoke", &id]);
    assert_eq!(revoked.status.code(), Some(0), "{}", stderr(&revoked));
    assert_eq!(explain()["outcome"], "denied");
}
