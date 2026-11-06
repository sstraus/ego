use crate::support;

use support::{Ego, Plan, Reply, Stub};

// Catches: sequential interactive EOF overwrites the failed turn with exit 0.
#[test]
fn redirected_eof_preserves_failed_turn_status() {
    let cases = [
        ("provider", Reply::Status(503), 4, "Provider"),
        ("response body", Reply::TruncatedSse, 4, "Provider"),
        ("usage", Reply::Status(400), 2, "Usage"),
        (
            "context overflow",
            Reply::Sse("data: {\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"window exceeded\"}}\n\n".to_owned()),
            4,
            "ContextOverflow",
        ),
    ];
    let mut failures = Vec::new();
    for (name, reply, code, reason) in cases {
        let stub = Stub::start(Plan {
            completions: vec![reply],
            ..Plan::default()
        });
        let result = Ego::against(&stub)
            .conversing()
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--retry",
                "0",
                "-",
            ])
            .stdin("Finish this task")
            .run();
        let payloads = result.durable_payloads();
        let settled = payloads.iter().any(|payload| {
            let event = &payload["event"]["kind"];
            event["kind"] == "TurnCompleted"
                && event["ok"] == false
                && event["exit_reason"] == reason
        });
        if result.code != Some(code) || !settled || !result.stdout.trim().is_empty() {
            failures.push(format!(
                "{name}: exit={:?}, expected={code}, durable failed {reason}={settled}, stdout={:?}, stderr={}",
                result.code, result.stdout, result.stderr
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Catches: a fix to redirected EOF also changes normal human terminal closure.
#[test]
fn human_eof_after_provider_failure_still_closes_normally() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(503)],
        ..Plan::default()
    });
    let result = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--retry",
            "0",
        ])
        .run_pty("Finish this task");
    assert_eq!(result.code, Some(0), "{}", result.stdout);
    assert_eq!(stub.generations().len(), 1, "the failed turn must run");
}

// Catches: explicit headless stdin inherits interactive recovery or exit masking.
#[test]
fn headless_stdin_failure_keeps_finite_retry_default_and_provider_exit() {
    // A lost stream, not a capacity refusal, so the default retries wait
    // seconds rather than minutes.
    let stub = Stub::start(Plan {
        completions: vec![Reply::TruncatedSse],
        ..Plan::default()
    });
    let result = Ego::against(&stub)
        .args(["--model", "ollama/qwen3", "--sandbox", "off", "-"])
        .stdin("Finish this task")
        .run();
    assert_eq!(result.code, Some(4), "{}", result.stderr);
    assert_eq!(
        stub.generations().len(),
        4,
        "headless defaults to three additional attempts, not interactive recovery"
    );
}

// Catches: preserving redirected failures accidentally makes successful EOF fail.
#[test]
fn redirected_dash_success_still_exits_zero() {
    let stub = Stub::start(Plan::default());
    let result = Ego::against(&stub)
        .conversing()
        .args(["--model", "ollama/qwen3", "--sandbox", "off", "-"])
        .stdin("Finish this task")
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(result.stdout.trim(), "It is done.");
    assert_eq!(stub.generations().len(), 1);
}
