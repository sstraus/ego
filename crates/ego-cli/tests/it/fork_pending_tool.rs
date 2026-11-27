//! One-command fork while the real source still owns an unfinished tool call.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::support::{FIXTURE_SESSION_ID, Places, Plan, Reply, Running, Stub, text, tool_call};

struct SourceRun {
    process: Option<Running>,
    release: PathBuf,
}

impl Drop for SourceRun {
    fn drop(&mut self) {
        // Allow the fixture tool to exit even if an assertion failed.
        let _ = fs::write(&self.release, "release");
        if let Some(process) = self.process.as_mut() {
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
    }
}

// Catches: the combined fork refuses an active owner or copies an outstanding
// tool call into the fork, so the first request cannot continue a valid history.
#[test]
fn resume_fork_excludes_a_real_pending_exec_call() {
    let places = Places::new();
    let started = places.workspace_path().join("exec-entered");
    let release = places.workspace_path().join("exec-release");
    let args = serde_json::json!({
        "argv": ["/bin/sh", "-c", ": > \"$1\"; while [ ! -f \"$2\" ]; do sleep 0.05; done; printf 'source tool completed'", "fixture", started, release],
        "timeout_ms": 20_000,
    });
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("settled source answer")),
            Reply::Sse(tool_call(
                "source-unfinished-exec",
                "exec",
                &args.to_string(),
            )),
            Reply::Sse(text("fork answer")),
            Reply::Sse(text("source after tool")),
        ],
        ..Plan::default()
    });
    let initial = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--session-id",
            FIXTURE_SESSION_ID,
            "--output",
            "json",
            "settled source prompt",
        ])
        .run();
    assert_eq!(initial.code, Some(0), "{}", initial.stderr);
    let mut source = SourceRun {
        process: Some(
            places
                .ego(&stub)
                .subcommand("resume")
                .args([
                    FIXTURE_SESSION_ID,
                    "--sandbox",
                    "off",
                    "--mode",
                    "yolo",
                    "--output",
                    "json",
                    "unsettled source prompt",
                ])
                .spawn(),
        ),
        release,
    };
    let journal = places
        .home_path()
        .join("sessions")
        .join(FIXTURE_SESSION_ID)
        .join("events.jsonl");
    let until = Instant::now() + Duration::from_secs(8);
    let before = loop {
        let bytes = fs::read(&journal).unwrap();
        if started.is_file()
            && String::from_utf8_lossy(&bytes).lines().any(|line| {
                let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
                    return false;
                };
                let event = &record["payload"]["event"]["kind"];
                event["kind"] == "ToolCallStarted" && event["call_id"] == "source-unfinished-exec"
            })
        {
            break bytes;
        }
        assert!(
            source.process.as_mut().unwrap().is_running(),
            "source exited before pending exec"
        );
        assert!(
            Instant::now() < until,
            "exec marker present: {}; provider requests: {}; no matching durable tool start found",
            started.is_file(),
            stub.generations().len()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!source.release.exists());
    assert_eq!(
        stub.generations().len(),
        2,
        "source cannot advance past gated exec"
    );

    let fork = places
        .ego(&stub)
        .subcommand("resume")
        .args([
            FIXTURE_SESSION_ID,
            "--fork",
            "--sandbox",
            "off",
            "--timeout",
            "8s",
            "--output",
            "json",
            "fork prompt during source exec",
        ])
        .run();
    assert_eq!(fork.code, Some(0), "{}", fork.stderr);
    let report = fork.report();
    assert_ne!(report["session"].as_str().unwrap(), FIXTURE_SESSION_ID);
    assert_eq!(report["result"], "fork answer");
    assert!(source.process.as_mut().unwrap().is_running());
    assert_eq!(
        fs::read(&journal).unwrap(),
        before,
        "fork changed the active source journal"
    );

    let requests = stub.generations();
    assert_eq!(requests.len(), 3);
    let fork_request = requests[2].to_string();
    for text in [
        "settled source prompt",
        "settled source answer",
        "fork prompt during source exec",
    ] {
        assert!(
            fork_request.contains(text),
            "missing {text}: {fork_request}"
        );
    }
    for text in [
        "unsettled source prompt",
        "source-unfinished-exec",
        "source tool completed",
    ] {
        assert!(
            !fork_request.contains(text),
            "inherited {text}: {fork_request}"
        );
    }

    fs::write(&source.release, "release").unwrap();
    let finished_source = source.process.take().unwrap().finish();
    assert_eq!(finished_source.code, Some(0), "{}", finished_source.stderr);
    let requests = stub.generations();
    assert_eq!(requests.len(), 4);
    let continuation = requests[3].to_string();
    assert!(continuation.contains("source tool completed"));
    assert!(!continuation.contains("fork prompt during source exec"));
}
