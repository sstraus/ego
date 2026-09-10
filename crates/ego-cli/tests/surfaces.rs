//! RED contract for M4 S8: inspection surfaces and executable agent files.
//!
//! These tests exercise the process boundary.  The catalogue shown by an
//! inspection command must be the immutable snapshot used by invocation, and
//! reload must publish a complete replacement rather than mutate a live view.

use serde_json::Value;

mod support;

use support::{Ego, Plan, Reply, Stub, text};

struct Fixture {
    _home: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    stub: Stub,
}

impl Fixture {
    fn new() -> Self {
        Self {
            _home: tempfile::tempdir().expect("home"),
            _workspace: tempfile::tempdir().expect("workspace"),
            stub: Stub::start(Plan {
                completions: vec![Reply::Sse(text("{\"ok\":true}"))],
                ..Plan::default()
            }),
        }
    }

    fn ego(&self) -> Ego {
        Ego::at(self._home.path(), self._workspace.path(), Some(&self.stub))
    }
}

#[test]
fn hooks_inspection_is_read_only_versioned_and_exposes_refusal_facts() {
    let fixture = Fixture::new();
    fixture.ego().home_file("config.toml", "[[hooks]]\nevent = 'PreToolUse'\nmatcher = 'read'\n[hooks.handler]\ntype = 'command'\ncommand = '/bin/true'\n");
    fixture.ego().file(
        ".ego/config.toml",
        "this project config is deliberately malformed = [",
    );
    let listed = fixture
        .ego()
        .subcommand("hooks")
        .args(["ls", "--json"])
        .run();
    assert_eq!(listed.code, Some(0), "{}", listed.stderr);
    let json: Value = serde_json::from_str(&listed.stdout).expect("versioned hooks JSON");
    assert_eq!(json["v"], 1);
    let mut top_keys: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    top_keys.sort_unstable();
    assert_eq!(top_keys, ["generation", "hooks", "v"]);
    let hook = &json["hooks"][0];
    let mut row_keys: Vec<&str> = hook
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    row_keys.sort_unstable();
    assert_eq!(
        row_keys,
        [
            "activation",
            "digest",
            "enabled",
            "handler_kind",
            "id",
            "lifecycle",
            "logical_location",
            "matcher",
            "ordinal",
            "refusal_reason",
            "source_authority"
        ]
    );
    assert_eq!(hook["handler_kind"], "command");
    assert_eq!(hook["enabled"], true);
    assert!(hook["refusal_reason"].is_null());
    assert!(!listed.stdout.contains("/bin/true"));
    assert!(!listed.stdout.contains("secret"));
    let before = std::fs::read(fixture._home.path().join("config.toml")).unwrap();
    let _ = fixture.ego().subcommand("hooks").args(["ls"]).run();
    assert_eq!(
        std::fs::read(fixture._home.path().join("config.toml")).unwrap(),
        before
    );
    assert!(!listed.stdout.contains("deliberately malformed"));
}

#[test]
fn invalid_hook_candidate_is_rejected_and_inspection_is_read_only() {
    let fixture = Fixture::new();
    fixture.ego().home_file(
        "config.toml",
        "[[hooks]]\nevent = 'PreToolUse'\n[hooks.handler]\ntype = 'prompt'\nprompt = 'ok'\n",
    );
    let first = fixture
        .ego()
        .subcommand("hooks")
        .args(["ls", "--json"])
        .run();
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    assert!(first.stdout.contains("PreToolUse"));
    let before = std::fs::read(fixture._home.path().join("config.toml")).unwrap();
    fixture.ego().home_file("config.toml", "not valid = [");
    let second = fixture
        .ego()
        .subcommand("hooks")
        .args(["ls", "--json"])
        .run();
    assert_ne!(second.code, Some(0));
    assert_eq!(
        std::fs::read(fixture._home.path().join("config.toml")).unwrap(),
        b"not valid = ["
    );
    assert_ne!(before, b"not valid = [");
}

#[test]
fn executable_ego_shebang_uses_real_run_pipeline_and_relative_schema() {
    let fixture = Fixture::new();
    fixture.ego().home_file("config.toml", "thinking = 'off'\n");
    fixture.ego().file("agents/schema.json", "{}");
    let path = fixture.ego().file("agents/demo.ego", "#!/usr/bin/env -S ego run -f\n---\nversion: 1\nmodel: ollama/qwen3\nsandbox: off\nschema: '@schema.json'\n---\nSay hello.\n");
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
    }
    let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_ego"))
        .parent()
        .expect("ego binary directory");
    let output = std::process::Command::new(&path)
        .env("EGO_HOME", fixture._home.path())
        .env("OLLAMA_HOST", &fixture.stub.address)
        .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.stub.generations().len(), 1);
}
