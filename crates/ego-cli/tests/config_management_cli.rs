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
    // Adaptive grants the child temp root write access, so it must not contain HOME.
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("fixture home"),
            workspace: tempfile::tempdir().expect("fixture workspace"),
            temp: tempfile::tempdir().expect("fixture temp root"),
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
            .env("TMPDIR", self.temp.path())
            .current_dir(self.workspace.path())
            .args(args);
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
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

// Catches: `config set` receipt exposes the internal publication word, and an
// unknown key error that does not say which key was wrong (regression case).
#[test]
fn set_prints_a_plain_receipt_and_an_unknown_key_error_names_the_key() {
    let fixture = Fixture::new();
    let set = fixture.run(&["config", "set", "model=\"ollama/qwen3\""]);
    assert_eq!(set.status.code(), Some(0), "stderr: {}", error(&set));
    assert_eq!(text(&set), "Saved model = \"ollama/qwen3\" in user.\n");
    let profile = fixture.run(&["config", "set", "--profile", "ci", "model=\"ollama/qwen3\""]);
    assert_eq!(
        text(&profile),
        "Saved model = \"ollama/qwen3\" in user:profile=\"ci\".\n"
    );
    let same = fixture.run(&["config", "set", "model=\"ollama/qwen3\""]);
    assert_eq!(
        text(&same),
        "Unchanged: model = \"ollama/qwen3\" in user.\n"
    );
    let unset = fixture.run(&["config", "unset", "model"]);
    assert_eq!(text(&unset), "Removed model from user.\n");
    let unset_again = fixture.run(&["config", "unset", "model"]);
    assert_eq!(text(&unset_again), "Unchanged: model is not set in user.\n");

    let unknown = fixture.run(&["config", "set", "modle=\"x\""]);
    assert_usage_failure(&unknown, "unknown config key `modle`");
    let unknown_unset = fixture.run(&["config", "unset", "modle"]);
    assert_usage_failure(&unknown_unset, "unknown config key `modle`");
}

#[test]
fn management_is_persistent_user_authority_and_ignores_ego_profile() {
    let fixture = Fixture::new();
    let set = fixture.run_with_env(
        &["config", "set", "model=\"ollama/qwen3\""],
        &[("EGO_PROFILE", "ci")],
    );
    assert_eq!(set.status.code(), Some(0), "stderr: {}", error(&set));
    assert_eq!(text(&set), "Saved model = \"ollama/qwen3\" in user.\n");

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
        "ui.statusline.template='{{ prompt }}'",
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
        "ui.statusline.template[0]=\"context\"",
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

    fixture.write_config("model = \"ollama/qwen3\"\nthinking = \"high\"\nbare = true\n[ui.statusline]\ntemplate = '{{ prompt }}'\n");
    let listed = fixture.run(&["config", "ls"]);
    assert_eq!(listed.status.code(), Some(0), "stderr: {}", error(&listed));
    assert_eq!(
        text(&listed),
        "model\t\"ollama/qwen3\"\nthinking\t\"high\"\nbare\ttrue\nui.statusline.template\t\"{{ prompt }}\"\n"
    );
    let json = fixture.run(&["config", "ls", "--json"]);
    assert_eq!(json.status.code(), Some(0));
    assert_eq!(
        text(&json),
        "{\"v\":1,\"scope\":\"user\",\"profile\":null,\"values\":{\"model\":\"ollama/qwen3\",\"thinking\":\"high\",\"bare\":true,\"ui.statusline.template\":\"{{ prompt }}\"}}\n"
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
    assert_eq!(text(&unset), "Removed model from user:profile=\"ci\".\n");
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
        "Saved session_dir = \"/tmp/sessions\" in user:profile=\"ci\".\n"
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
        "Unchanged: session_dir = \"/tmp/sessions\" in user:profile=\"ci\".\n"
    );
    let unset = fixture.run(&["config", "unset", "--profile", "ci", "session_dir"]);
    assert_eq!(unset.status.code(), Some(0));
    assert_eq!(
        text(&unset),
        "Removed session_dir from user:profile=\"ci\".\n"
    );
    let missing = fixture.run(&["config", "unset", "--profile", "ci", "session_dir"]);
    assert_eq!(missing.status.code(), Some(0));
    assert_eq!(
        text(&missing),
        "Unchanged: session_dir is not set in user:profile=\"ci\".\n"
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

// Catches: absent roots become [], hiding the distinction from workspace-only access.
#[test]
fn roots_json_distinguishes_absent_empty_and_populated_values() {
    let fixture = Fixture::new();
    for (config, expected) in [
        ("model='ollama/qwen3'\n".to_owned(), serde_json::Value::Null),
        ("roots=[]\n".to_owned(), serde_json::json!([])),
        (
            format!(
                "roots=[{{path='{}',access='read'}}]\n",
                fixture.workspace.path().display()
            ),
            serde_json::json!([{"path": fixture.workspace.path(), "access": "read"}]),
        ),
    ] {
        fixture.write_config(&config);
        let output = fixture.run(&["roots", "ls", "--json"]);
        assert!(output.status.success(), "{}", error(&output));
        assert!(output.stderr.is_empty(), "{}", error(&output));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["values"]["roots"], expected, "{config}");
    }
}

// Catches: roots are not writable through the managed vocabulary or add/rm overwrite sibling fields.
#[test]
fn roots_commands_update_exact_user_target_without_erasing_other_settings() {
    let fixture = Fixture::new();
    fixture.write_config("model='ollama/qwen3'\n");
    let path = fixture.workspace.path().to_str().unwrap();
    for args in [
        vec!["roots", "add", "--access", "read-write", path],
        vec!["roots", "add", "--access", "read-write", path],
        vec!["roots", "add", "--profile", "strict", path],
    ] {
        let output = fixture.run(&args);
        assert!(output.status.success(), "{}", error(&output));
    }
    let output = fixture.run(&["roots", "ls"]);
    assert!(text(&output).contains("read-write"));
    let original = std::fs::read_to_string(fixture.config_path()).unwrap();
    assert!(original.contains("ollama/qwen3"));
    assert!(
        fixture
            .run(&["roots", "rm", "--profile", "strict", path])
            .status
            .success()
    );
    assert!(
        fixture
            .run(&["config", "set", "--profile", "strict", "roots=[]"])
            .status
            .success()
    );
    let output = fixture.run(&["roots", "ls", "--profile", "strict"]);
    assert!(text(&output).contains("No roots are stored"));
    let before_invalid = std::fs::read_to_string(fixture.config_path()).unwrap();
    let output = fixture.run(&["config", "set", "roots=[{path='relative',access='read'}]"]);
    assert!(!output.status.success());
    assert_eq!(
        std::fs::read_to_string(fixture.config_path()).unwrap(),
        before_invalid
    );
}

#[path = "config_management_cli/auth_exec_paths.rs"]
mod auth_exec_paths;

#[path = "config_management_cli/batch.rs"]
mod batch;

#[path = "config_management_cli/conditional.rs"]
mod conditional;

// Catches: keyboard settings cannot round-trip through managed configuration or escape user scope.
#[test]
fn keyboard_preferences_round_trip_as_user_only_settings() {
    let fixture = Fixture::new();
    for assignment in ["ui.vim=true", "ui.keybindings={model=['Ctrl+P'],expand=[]}"] {
        let saved = fixture.run(&["config", "set", assignment]);
        assert_eq!(saved.status.code(), Some(0), "{}", error(&saved));
        assert_usage_failure(
            &fixture.run(&["config", "set", "--profile", "dev", assignment]),
            "not available",
        );
    }
    assert_eq!(text(&fixture.run(&["config", "get", "ui.vim"])), "true\n");
    let binding = fixture.run(&["config", "get", "ui.keybindings", "--json"]);
    assert_eq!(binding.status.code(), Some(0), "{}", error(&binding));
    let value: serde_json::Value = serde_json::from_slice(&binding.stdout).unwrap();
    assert_eq!(value["values"]["ui.keybindings"]["model"][0], "Ctrl+P");
    assert_eq!(
        value["values"]["ui.keybindings"]["expand"],
        serde_json::json!([])
    );
    let disk = std::fs::read_to_string(fixture.config_path()).unwrap();
    for (assignment, reason) in [
        ("ui.keybindings={model=['Ctrl+S']}", "reserved"),
        ("ui.keybindings={model=['Alt+O']}", "conflicts"),
        ("ui.keybindings={typo=['F2']}", "unknown keybinding action"),
    ] {
        for args in [
            vec!["config", "set", assignment],
            vec!["config", "batch", "--set", assignment],
        ] {
            assert_usage_failure(&fixture.run(&args), reason);
            assert_eq!(
                std::fs::read_to_string(fixture.config_path()).unwrap(),
                disk
            );
        }
    }
    for key in ["ui.vim", "ui.keybindings"] {
        let unset = fixture.run(&["config", "unset", key]);
        assert_eq!(unset.status.code(), Some(0), "{}", error(&unset));
    }
    assert!(text(&fixture.run(&["config", "ls"])).is_empty());
}

// Catches: a handwritten conflicting keymap starts the terminal or reaches model admission.
#[test]
fn invalid_keyboard_preferences_are_refused_before_terminal_activation() {
    let fixture = Fixture::new();
    fixture.write_config("[ui]\nvim=true\nkeybindings={model=['Alt+O']}\n");
    std::fs::create_dir(fixture.workspace.path().join(".ego")).unwrap();
    std::fs::write(
        fixture.workspace.path().join(".ego/config.toml"),
        "not valid TOML",
    )
    .unwrap();
    let refused = fixture.run(&["--vim"]);
    assert_usage_failure(&refused, "conflicts");
    assert!(!error(&refused).contains("model admission"));
    assert!(
        !refused.stdout.contains(&0x1b),
        "terminal activation emitted control sequences"
    );
}
