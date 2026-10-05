//! contract for regression case's final `ego config` management surface.
//!
//! These tests deliberately exercise the installed binary as separate
//! processes. The atomic crash and concurrency contract is covered by the
//! existing core/tools transaction tests; this module proves the CLI seam and
//! its durable authority across process boundaries.

use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("fixture home"),
            workspace: tempfile::tempdir().expect("fixture workspace"),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
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
        command.output().expect("ego child process")
    }

    fn config_path(&self) -> PathBuf {
        self.home.path().join("config.toml")
    }

    fn write_config(&self, contents: &str) {
        std::fs::write(self.config_path(), contents).expect("fixture config")
    }
}

fn text(output: &Output) -> &str {
    std::str::from_utf8(&output.stdout).expect("UTF-8 stdout")
}

fn error(output: &Output) -> &str {
    std::str::from_utf8(&output.stderr).expect("UTF-8 stderr")
}

fn assert_usage_failure(output: &Output, needle: &str) {
    assert_eq!(output.status.code(), Some(2), "stderr: {}", error(output));
    assert!(output.stdout.is_empty(), "usage failure emitted stdout");
    assert!(
        error(output).contains(needle),
        "missing `{needle}` in {}",
        error(output)
    );
}

#[test]
fn root_grammar_accepts_only_the_frozen_config_forms() {
    let fixture = Fixture::new();
    for args in [
        &["config", "get", "model", "--json"] as &[&str],
        &["config", "get", "--profile", "ci", "--json", "model"],
        &["config", "get", "--profile", "ci", "model"],
        &["config", "set", "--profile", "ci", "model=\"ollama/qwen3\""],
        &["config", "unset", "--profile", "ci", "model"],
        &["config", "ls", "--profile", "ci", "--json"],
    ] {
        let output = fixture.run(args);
        assert_ne!(
            output.status.code(),
            Some(2),
            "accepted grammar rejected: {args:?}: {}",
            error(&output)
        );
    }
    for args in [
        &["config", "set", "--json", "model=\"ollama/qwen3\""] as &[&str],
        &["config", "unset", "--json", "model"],
        &["config", "ls", "model"],
        &["config", "get", "model", "extra"],
        &["config", "set", "model=\"one\"", "mode=\"plan\""],
        &["config", "unset"],
    ] {
        assert_usage_failure(&fixture.run(args), "Usage");
    }
}

#[test]
fn management_is_persistent_user_authority_and_ignores_ego_profile() {
    let fixture = Fixture::new();
    let set = fixture.run_with_env(
        &["config", "set", "model=\"ollama/qwen3\""],
        &[("EGO_PROFILE", "ci")],
    );
    assert_eq!(set.status.code(), Some(0), "stderr: {}", error(&set));
    assert_eq!(
        text(&set).split('\t').collect::<Vec<_>>(),
        [
            "changed",
            "user",
            "model",
            "\"ollama/qwen3\"",
            "published_durably\n"
        ]
    );

    let get = fixture.run_with_env(&["config", "get", "model"], &[("EGO_PROFILE", "ci")]);
    assert_eq!(get.status.code(), Some(0), "stderr: {}", error(&get));
    assert_eq!(text(&get), "\"ollama/qwen3\"\n");
    let profile = fixture.run(&["config", "get", "--profile", "ci", "model"]);
    assert_eq!(profile.status.code(), Some(1));
    assert!(text(&profile).is_empty());
    assert!(error(&profile).contains("model"));

    let profile_set = fixture.run(&[
        "config",
        "set",
        "--profile",
        "ci",
        "model=\"ollama/llama3\"",
    ]);
    assert_eq!(
        profile_set.status.code(),
        Some(0),
        "stderr: {}",
        error(&profile_set)
    );
    assert_eq!(
        fixture.run(&["config", "get", "model"]).stdout,
        b"\"ollama/qwen3\"\n"
    );
    assert_eq!(
        fixture
            .run(&["config", "get", "--profile", "ci", "model"])
            .stdout,
        b"\"ollama/llama3\"\n"
    );
}

#[test]
fn closed_keys_and_values_are_typed_and_forbidden_shapes_are_rejected() {
    let fixture = Fixture::new();
    for assignment in [
        "model=\"ollama/qwen3\"",
        "thinking=\"high\"",
        "mode=\"plan\"",
        "sandbox=\"workspace\"",
        "budget=\"10$\"",
        "bare=true",
        "permissions.judge='ollama/qwen3'",
        "ui.statusline.segments=[\"model\",\"context\"]",
    ] {
        let output = fixture.run(&["config", "set", assignment]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "{assignment}: {}",
            error(&output)
        );
    }
    for assignment in [
        "permissions.allow=['read(src/**)']",
        "auth.token=\"SECRET\"",
        "mcp.foo=\"bar\"",
        "hooks.pre=\"echo\"",
        "future.key=\"x\"",
        "ui.statusline.segments[0]=\"context\"",
        "ui={statusline={segments=[\"context\"]}}",
        "model=null",
    ] {
        let output = fixture.run(&["config", "set", assignment]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "forbidden assignment accepted: {assignment}"
        );
        assert!(output.stdout.is_empty());
    }
    for assignment in [
        "session_dir=\"/tmp/sessions\"",
        "artifact_dir=\"/tmp/artifacts\"",
    ] {
        let output = fixture.run(&["config", "set", assignment]);
        assert_eq!(
            output.status.code(),
            Some(2),
            "base accepted CI key: {assignment}"
        );
    }
    for assignment in [
        "session_dir=\"/tmp/sessions\"",
        "artifact_dir=\"/tmp/artifacts\"",
    ] {
        let output = fixture.run(&["config", "set", "--profile", "ci", assignment]);
        assert_eq!(
            output.status.code(),
            Some(0),
            "CI key rejected: {assignment}: {}",
            error(&output)
        );
    }
}

#[test]
fn missing_and_empty_reads_have_their_exact_text_and_json_contracts() {
    let fixture = Fixture::new();
    let missing = fixture.run(&["config", "get", "model"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty());
    assert!(error(&missing).contains("model"));
    let empty = fixture.run(&["config", "ls"]);
    assert_eq!(empty.status.code(), Some(0), "stderr: {}", error(&empty));
    assert!(empty.stdout.is_empty());
    let empty_json = fixture.run(&["config", "ls", "--json"]);
    assert_eq!(
        empty_json.status.code(),
        Some(0),
        "stderr: {}",
        error(&empty_json)
    );
    assert_eq!(
        empty_json.stdout,
        b"{\"v\":1,\"scope\":\"user\",\"profile\":null,\"values\":{}}\n"
    );

    fixture.write_config("model = \"ollama/qwen3\"\nthinking = \"high\"\nbare = true\n[ui.statusline]\nsegments = [\"model\", \"context\"]\n");
    let listed = fixture.run(&["config", "ls"]);
    assert_eq!(listed.status.code(), Some(0), "stderr: {}", error(&listed));
    assert_eq!(
        text(&listed),
        "model\t\"ollama/qwen3\"\nthinking\t\"high\"\nbare\ttrue\nui.statusline.segments\t[\"model\", \"context\"]\n"
    );
    let json = fixture.run(&["config", "ls", "--json"]);
    assert_eq!(json.status.code(), Some(0));
    assert_eq!(
        text(&json),
        "{\"v\":1,\"scope\":\"user\",\"profile\":null,\"values\":{\"model\":\"ollama/qwen3\",\"thinking\":\"high\",\"bare\":true,\"ui.statusline.segments\":[\"model\",\"context\"]}}\n"
    );
}

#[test]
fn profile_json_isolated_and_profile_removal_prunes_empty_container() {
    let fixture = Fixture::new();
    let set = fixture.run(&["config", "set", "--profile", "ci", "model=\"ollama/qwen3\""]);
    assert_eq!(set.status.code(), Some(0), "stderr: {}", error(&set));
    let json = fixture.run(&["config", "ls", "--profile", "ci", "--json"]);
    assert_eq!(json.stdout, b"{\"v\":1,\"scope\":\"user\",\"profile\":\"ci\",\"values\":{\"model\":\"ollama/qwen3\"}}\n");
    let unset = fixture.run(&["config", "unset", "--profile", "ci", "model"]);
    assert_eq!(unset.status.code(), Some(0), "stderr: {}", error(&unset));
    assert_eq!(
        text(&unset),
        "changed\tuser:profile=\"ci\"\tmodel\t<absent>\tpublished_durably\n"
    );
    let bytes = std::fs::read_to_string(fixture.config_path()).expect("config remains readable");
    assert!(
        !bytes.contains("[profile.ci]"),
        "empty profile container survived: {bytes}"
    );
}

#[test]
fn mutation_receipts_escape_fields_and_report_changed_unchanged_and_unset_noop() {
    let fixture = Fixture::new();
    let first = fixture.run(&[
        "config",
        "set",
        "--profile",
        "ci",
        "session_dir=\"/tmp/sessions\"",
    ]);
    assert_eq!(first.status.code(), Some(0), "stderr: {}", error(&first));
    assert_eq!(
        text(&first),
        "changed\tuser:profile=\"ci\"\tsession_dir\t\"/tmp/sessions\"\tpublished_durably\n"
    );
    let same = fixture.run(&[
        "config",
        "set",
        "--profile",
        "ci",
        "session_dir=\"/tmp/sessions\"",
    ]);
    assert_eq!(same.status.code(), Some(0));
    assert_eq!(
        text(&same),
        "unchanged\tuser:profile=\"ci\"\tsession_dir\t\"/tmp/sessions\"\tnot_published\n"
    );
    let unset = fixture.run(&["config", "unset", "--profile", "ci", "session_dir"]);
    assert_eq!(unset.status.code(), Some(0));
    assert_eq!(
        text(&unset),
        "changed\tuser:profile=\"ci\"\tsession_dir\t<absent>\tpublished_durably\n"
    );
    let missing = fixture.run(&["config", "unset", "--profile", "ci", "session_dir"]);
    assert_eq!(missing.status.code(), Some(0));
    assert_eq!(
        text(&missing),
        "unchanged\tuser:profile=\"ci\"\tsession_dir\t<absent>\tnot_published\n"
    );
}

#[test]
fn rejected_values_and_usage_emit_no_receipt_and_keep_secrets_out_of_stderr() {
    let fixture = Fixture::new();
    for args in [
        &["config", "set", "model='unterminated"] as &[&str],
        &["config", "set", "permissions.allow=[\"read(src/**)\"]"],
        &["config", "set", "model=SECRET-DO-NOT-LEAK", "extra"],
    ] {
        let output = fixture.run(args);
        assert!(output.status.code().is_some_and(|code| code != 0));
        assert!(output.stdout.is_empty());
        assert!(error(&output).contains("config") || error(&output).contains("model"));
        assert!(
            !error(&output).contains("SECRET-DO-NOT-LEAK"),
            "secret leaked: {}",
            error(&output)
        );
    }
}

#[test]
fn profile_names_follow_the_shared_validation_boundary() {
    let fixture = Fixture::new();
    for profile in ["", " leading", "trailing ", "line\nfeed"] {
        let output = fixture.run(&["config", "ls", "--profile", profile]);
        assert_usage_failure(&output, "profile");
    }
    let valid = "profile with spaces";
    let output = fixture.run(&[
        "config",
        "set",
        "--profile",
        valid,
        "model=\"ollama/qwen3\"",
    ]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", error(&output));
}

// Catches: opt-in child controls are writable but ignored by effective inspection,
// or invalid allowlist names overwrite the existing user config.
#[test]
fn child_environment_and_network_controls_are_durable_and_strict() {
    let fixture = Fixture::new();
    fixture.write_config("# preserve this comment\nmodel = 'provider/example'\n");
    for assignment in [
        "exec.env='core'",
        "network='off'",
        "exec.env_allowlist=['PATH']",
    ] {
        let output = fixture.run(&["config", "set", assignment]);
        assert!(output.status.success(), "{}", error(&output));
    }
    let output = fixture.run(&["config", "get", "exec.env"]);
    assert!(text(&output).contains("core"));
    let output = fixture.run(&["config", "ls", "--effective", "--json"]);
    assert!(output.status.success(), "{}", error(&output));
    let effective: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(effective["network"], "offline");
    let before = std::fs::read(fixture.config_path()).unwrap();
    assert_usage_failure(
        &fixture.run(&["config", "set", "exec.env_allowlist=['BAD=NAME']"]),
        "exec.env_allowlist",
    );
    assert_eq!(std::fs::read(fixture.config_path()).unwrap(), before);
    assert!(
        String::from_utf8(before)
            .unwrap()
            .contains("# preserve this comment")
    );
    for key in ["exec.env", "exec.env_allowlist", "network"] {
        assert!(fixture.run(&["config", "unset", key]).status.success());
    }
    let output = fixture.run(&["config", "ls", "--effective", "--json"]);
    let effective: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(effective["network"], "online");
}
