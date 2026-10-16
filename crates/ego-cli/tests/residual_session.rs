//! Real CLI and ACP sessions against the existing loopback provider harness.
mod support;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use support::{AcpHost, Ego, Places, Plan, Reply, Stub, text, tool_call};

const CONFIG: &str = "model = 'ollama/qwen3'\nthinking = 'off'\nmode = 'auto'\nsandbox = 'adaptive'\nroots = []\n[permissions]\njudge = 'ollama/qwen3'\nallow = ['exec(*)']\n";

fn setup(places: &Places) -> (PathBuf, tempfile::TempDir) {
    std::fs::write(places.home_path().join("config.toml"), CONFIG).unwrap();
    // HOME and temporary writes must not overlap the protected EGO_HOME.
    let home = tempfile::tempdir().unwrap();
    let reports = home.path().join("reports");
    let scratch = home.path().join("temporary");
    std::fs::create_dir(&reports).unwrap();
    std::fs::create_dir(&scratch).unwrap();
    (
        std::fs::canonicalize(reports).unwrap().join("export.txt"),
        home,
    )
}

fn exec(id: &str) -> Reply {
    Reply::Sse(tool_call(
        id,
        "exec",
        &json!({
            "argv": ["/bin/sh", "-c", "printf exported >> \"$HOME/reports/export.txt\""],
            "cwd": null,
            "timeout_ms": 1000,
        })
        .to_string(),
    ))
}

fn verdict(target: &Path) -> Reply {
    Reply::Sse(text(
        &json!({
            "grant": "write", "path": target, "scope": "session",
            "reason": "the user requested this export",
        })
        .to_string(),
    ))
}

fn events(home: &Path) -> Vec<Value> {
    let file = std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("events.jsonl"))
        .find(|path| path.is_file())
        .expect("the session journal exists");
    std::fs::read_to_string(file)
        .unwrap()
        .lines()
        .filter_map(|line| {
            let record: Value = serde_json::from_str(line).unwrap();
            record.get("payload")?.get("event").cloned()
        })
        .collect()
}

fn assert_shared_judge(places: &Places, stub: &Stub, target: &Path) {
    let events = events(places.home_path());
    let decisions: Vec<_> = events
        .iter()
        .filter(|event| event["kind"]["kind"] == "SandboxResidualDecided")
        .collect();
    assert_eq!(decisions.len(), 1, "{events:?}");
    let decision = decisions[0];
    assert_eq!(decision["kind"]["path"], target.to_str().unwrap());
    assert_eq!(decision["kind"]["outcome"], "granted");
    assert_eq!(decision["kind"]["recommended"], "session");
    let judge = events
        .iter()
        .find(|event| {
            event["kind"]["kind"] == "GenerationStarted" && event["kind"]["role"] == "judge"
        })
        .expect("the shared production judge made the decision");
    assert_eq!(decision["kind"]["attempt"], judge["kind"]["attempt"]);
    let requests = stub.generations();
    let judge_request = &requests[1];
    assert!(judge_request["tools"].as_array().is_none_or(Vec::is_empty));
    assert!(judge_request.to_string().contains("residual_write"));
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        "exported",
        "grant must reach the next exec without replaying the first"
    );
}

// Catches: CLI creates separate executor/turn desks, or drops the grant before
// the next explicit exec, so the export never writes (or is judged twice).
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "Adaptive requires native macOS Seatbelt"
)]
fn cli_residual_judge_grant_reaches_the_next_explicit_exec() {
    let places = Places::new();
    let (target, home) = setup(&places);
    let stub = Stub::start(Plan {
        completions: vec![
            exec("first"),
            verdict(&target),
            exec("second"),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let result = Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
        .env("HOME", home.path().to_str().unwrap())
        .env("TMPDIR", home.path().join("temporary").to_str().unwrap())
        .args([
            "--output",
            "json",
            "Export the report; explicitly retry after a grant",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(stub.generations().len(), 4);
    assert_shared_judge(&places, &stub, &target);
}

// Catches: ACP reconstructs a fresh residual desk for each prompt, losing the
// first run's grant before the session's second run.
#[test]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "Adaptive requires native macOS Seatbelt"
)]
fn acp_residual_judge_grant_survives_into_the_next_session_run() {
    let places = Places::new();
    let (target, home) = setup(&places);
    let stub = Stub::start(Plan {
        completions: vec![
            exec("first"),
            verdict(&target),
            Reply::Sse(text("grant received")),
            exec("second"),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let mut host = AcpHost::adopt(
        Ego::at(places.home_path(), places.workspace_path(), Some(&stub))
            .subcommand("acp")
            .env("HOME", home.path().to_str().unwrap())
            .env("TMPDIR", home.path().join("temporary").to_str().unwrap())
            .spawn_conversing(),
    );
    host.initialize();
    let session = host.new_session(places.workspace_path().to_str().unwrap());
    for (index, prompt) in ["Export the report", "Run the export again"]
        .into_iter()
        .enumerate()
    {
        let response = host.request(
            "session/prompt",
            json!({
                "sessionId": session, "prompt": [{"type": "text", "text": prompt}],
            }),
        );
        assert!(response["result"].is_object(), "{response}");
        if index == 0 {
            assert!(
                !target.exists(),
                "judge grant automatically replayed a failed command"
            );
            assert_eq!(stub.generations().len(), 3);
        }
    }
    let stderr = host.finish();
    assert!(stderr.is_empty(), "{stderr}");
    assert_eq!(stub.generations().len(), 5);
    assert_shared_judge(&places, &stub, &target);
}
