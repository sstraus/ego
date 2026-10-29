//! The real CLI renders the core sanitized export.

use std::path::{Path, PathBuf};

use serde_json::Value;

mod support;

use support::{Ego, Plan, Reply, Stub, costing};

struct Fixture {
    home: tempfile::TempDir,
    workspace: tempfile::TempDir,
    stub: Stub,
}

impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().expect("fixture home"),
            workspace: tempfile::tempdir().expect("fixture workspace"),
            stub: Stub::start(Plan {
                completions: vec![Reply::Sse(costing("VISIBLE-RESULT", 11, 5))],
                ..Plan::default()
            }),
        }
    }

    fn command(&self) -> Ego {
        Ego::at(self.home.path(), self.workspace.path(), Some(&self.stub))
    }

    fn session_dir(&self) -> &Path {
        self.workspace.path()
    }

    fn seed(&self) {
        let run = self
            .command()
            .args([
                "--model",
                "ollama/qwen3",
                "--session-id",
                support::FIXTURE_SESSION_ID,
                "--sandbox",
                "off",
                "--thinking",
                "off",
                "SECRET-PROMPT",
            ])
            .run();
        assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    }

    fn export<const N: usize>(&self, args: [&str; N]) -> support::Finished {
        self.command().subcommand("export").args(args).run()
    }
}

fn canonical_json(fixture: &Fixture) -> support::Finished {
    fixture.export([
        support::FIXTURE_SESSION_ID,
        "--sanitize",
        "--format",
        "json",
    ])
}

#[test]
fn sanitized_json_export_is_versioned_with_exact_top_level_fields() {
    let fixture = Fixture::new();
    fixture.seed();
    let finished = canonical_json(&fixture);
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let value: Value = serde_json::from_str(&finished.stdout).expect("JSON export");
    let keys: Vec<&str> = value
        .as_object()
        .expect("bundle object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["v", "target", "records", "redactions"]);
    assert_eq!(value["v"], 1);
    assert_eq!(value["target"], support::FIXTURE_SESSION_ID);
    assert!(value["records"].is_array(), "{value}");
    assert!(value["redactions"].is_array(), "{value}");
}

#[test]
fn sanitized_export_structurally_excludes_adversarial_material() {
    let fixture = Fixture::new();
    fixture.seed();
    let finished = canonical_json(&fixture);
    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    for secret in [
        "SECRET-PROMPT",
        "VISIBLE-RESULT",
        "args",
        "env",
        "cwd",
        "\"reasoning\":",
        "raw_error",
        "authorization",
        "Bearer",
    ] {
        assert!(!finished.stdout.contains(secret), "{secret} leaked");
    }
}

#[test]
fn markdown_json_and_jsonl_are_deterministic_views_of_one_canonical_bundle() {
    let fixture = Fixture::new();
    fixture.seed();
    let json = canonical_json(&fixture);
    let jsonl = fixture.export([
        support::FIXTURE_SESSION_ID,
        "--sanitize",
        "--format",
        "jsonl",
    ]);
    let markdown = fixture.export([support::FIXTURE_SESSION_ID, "--sanitize", "--format", "md"]);
    for (format, result) in [("json", &json), ("jsonl", &jsonl), ("md", &markdown)] {
        assert_eq!(result.code, Some(0), "{format}: {}", result.stderr);
        assert!(result.stderr.is_empty(), "{format} diagnostics leaked");
    }
    assert_eq!(
        jsonl.stdout, json.stdout,
        "JSONL is not the canonical JSON line"
    );
    let json_text = json.stdout.trim_end_matches('\n');
    let prefix = format!("# ego sanitized export\n\n{}{}{}json\n", '`', '`', '`');
    let suffix = format!("\n{}{}{}\n", '`', '`', '`');
    assert_eq!(markdown.stdout, format!("{prefix}{json_text}{suffix}"));
}

#[test]
fn file_delivery_is_stdout_equivalent_and_identical_republication_is_safe() {
    let fixture = Fixture::new();
    fixture.seed();
    let path: PathBuf = fixture.session_dir().join("sanitized-bundle.json");
    let stdout = canonical_json(&fixture);
    assert_eq!(stdout.code, Some(0), "stderr: {}", stdout.stderr);
    let path_text = path.to_str().expect("UTF-8 fixture path");
    let first = fixture.export([
        support::FIXTURE_SESSION_ID,
        "--sanitize",
        "--format",
        "json",
        "--file",
        path_text,
    ]);
    assert_eq!(first.code, Some(0), "stderr: {}", first.stderr);
    assert!(first.stdout.is_empty(), "file mode wrote stdout");
    assert_eq!(
        std::fs::read(&path).expect("published export bytes"),
        stdout.stdout.as_bytes()
    );
    let second = fixture.export([
        support::FIXTURE_SESSION_ID,
        "--sanitize",
        "--format",
        "json",
        "--file",
        path_text,
    ]);
    assert_eq!(
        second.code,
        Some(0),
        "identical publication: {}",
        second.stderr
    );
    assert_eq!(
        std::fs::read(&path).expect("re-published export bytes"),
        stdout.stdout.as_bytes()
    );
}

#[test]
fn jsonl_file_delivery_is_a_usage_error() {
    let fixture = Fixture::new();
    fixture.seed();
    let path = fixture.session_dir().join("invalid-jsonl-export");
    let path_text = path.to_str().expect("UTF-8 fixture path");
    let finished = fixture.export([
        support::FIXTURE_SESSION_ID,
        "--sanitize",
        "--format",
        "jsonl",
        "--file",
        path_text,
    ]);
    assert_eq!(finished.code, Some(2), "stderr: {}", finished.stderr);
    assert!(finished.stderr.contains("jsonl"), "{}", finished.stderr);
    assert!(finished.stderr.contains("--file"), "{}", finished.stderr);
    assert!(finished.stdout.is_empty());
    assert!(!path.exists(), "usage error created an artifact");
}

#[test]
fn missing_lineage_or_evidence_fails_closed_without_emitting_raw_bytes() {
    let fixture = Fixture::new();
    let finished = fixture.export([
        support::FIXTURE_SESSION_ID,
        "--sanitize",
        "--format",
        "json",
    ]);
    assert_ne!(finished.code, Some(0), "missing source was exported");
    assert!(finished.stdout.is_empty(), "failure emitted bundle bytes");
    assert!(!finished.stderr.contains("SECRET"), "raw bytes leaked");
}

// Catches an exact value surviving JSON escapes or the markdown renderer.
#[test]
fn exports_mask_known_env_values_in_every_format() {
    let fixture = Fixture::new();
    fixture.seed();
    for format in ["json", "jsonl", "md"] {
        let finished = fixture
            .command()
            .subcommand("export")
            .args([support::FIXTURE_SESSION_ID, "--format", format])
            .env("EGO_TEST_SECRET", "VISIBLE-RESULT")
            .run();
        assert_eq!(finished.code, Some(0), "{}", finished.stderr);
        assert!(
            !finished.stdout.contains("VISIBLE-RESULT"),
            "format {format}"
        );
        assert!(finished.stdout.contains("[REDACTED]"), "format {format}");
    }
}

// Catches a child token reaching the next provider request or the event journal.
#[test]
#[cfg(unix)]
fn child_secret_is_masked_in_model_context_event_log_and_export() {
    const TOKEN: &str = "ego-child-token-NKO-unique";
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(support::tool_call(
                "secret-child",
                "exec",
                r#"{"argv":["/bin/sh","-c","printf '%s' \"$EGO_TEST_TOKEN\"; printf '%s' \"$EGO_TEST_TOKEN\" >&2"],"timeout_ms":5000}"#,
            )),
            Reply::Sse(support::text("done")),
        ],
        ..Plan::default()
    });
    let finished = Ego::at(home.path(), workspace.path(), Some(&stub))
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--thinking",
            "off",
            "--sandbox",
            "off",
            "--allow",
            "exec(/bin/sh *)",
            "Print the inherited token",
        ])
        .env("EGO_TEST_TOKEN", TOKEN)
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let requests = stub.generations();
    let context = requests.last().unwrap().to_string();
    assert!(context.contains("[REDACTED]"));
    assert!(!context.contains(TOKEN));
    let log = std::fs::read_to_string(
        home.path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .unwrap();
    assert!(log.contains("[REDACTED]"));
    assert!(!log.contains(TOKEN));
    let exported = Ego::at(home.path(), workspace.path(), None)
        .subcommand("export")
        .args([support::FIXTURE_SESSION_ID, "--format", "json"])
        .run();
    assert_eq!(exported.code, Some(0), "{}", exported.stderr);
    assert!(exported.stdout.contains("[REDACTED]"));
    assert!(!exported.stdout.contains(TOKEN));
}
