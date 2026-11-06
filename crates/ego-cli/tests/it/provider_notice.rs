//! A provider refusal as observed by the invoking shell.

use crate::support;

use support::{Ego, Plan, Reply, Stub};

// Catches: one refused generation is rendered by both the event sink and final
// termination diagnostic, or silently retried despite an explicit zero budget.
#[test]
fn headless_capacity_refusal_sends_one_request_and_prints_one_notice() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(503)],
        ..Plan::default()
    });
    let finished = Ego::against(&stub)
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--retry",
            "0",
            "Ask",
        ])
        .run();

    assert_eq!(finished.code, Some(4), "{}", finished.stderr);
    assert!(finished.stdout.is_empty(), "{}", finished.stdout);
    assert_eq!(stub.generations().len(), 1, "hidden provider retry");
    assert_eq!(
        finished
            .stderr
            .lines()
            .filter(|line| line.contains("503"))
            .count(),
        1,
        "one refusal must have one diagnostic: {}",
        finished.stderr
    );
    assert!(finished.stderr.contains("503"), "{}", finished.stderr);
}

// Catches: parsing the default correctly while execution drops it, or gives
// a headless process the unbounded interactive recovery horizon.
// A lost stream keeps the seconds-long backoff, so the default is observable
// without the minutes a capacity refusal waits.
#[test]
fn headless_default_exhausts_after_four_failed_requests() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::TruncatedSse],
        ..Plan::default()
    });
    let finished = Ego::against(&stub)
        .args(["--sandbox", "off", "--model", "ollama/qwen3", "Ask"])
        .run();
    assert_eq!(finished.code, Some(4), "{}", finished.stderr);
    assert_eq!(stub.generations().len(), 4, "{}", finished.stderr);
    assert!(finished.stdout.is_empty(), "{}", finished.stdout);
    assert_eq!(
        finished
            .stderr
            .matches("connection problem, retrying")
            .count(),
        3
    );
}

// Catches: every provider-reported transient status taken as a capacity
// refusal, so a persistent generic 500 makes a headless run wait minutes
// before it retries.
#[test]
fn a_generic_server_error_keeps_the_seconds_long_backoff() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(500)],
        ..Plan::default()
    });
    let finished = Ego::against(&stub)
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--retry",
            "1",
            "Ask",
        ])
        .run();
    assert_eq!(finished.code, Some(4), "{}", finished.stderr);
    assert_eq!(stub.generations().len(), 2, "{}", finished.stderr);
    // The first local backoff is one second, less up to 25% jitter.
    assert!(
        ["0.7s", "0.8s", "0.9s", "1.0s"].iter().any(|delay| finished
            .stderr
            .contains(&format!("connection problem, retrying in {delay}"))),
        "{}",
        finished.stderr
    );
    assert!(
        !finished.stderr.contains("provider at capacity"),
        "{}",
        finished.stderr
    );
}

/// A provider refusal streamed after status 200, with a field that names an
/// account.
fn streamed_refusal() -> Reply {
    Reply::Sse(
        "data: {\"error\":{\"message\":\"at capacity\",\"type\":\"overloaded_error\"},\
         \"user\":\"acct-private\"}\n\n"
            .to_string(),
    )
}

fn refused_with_log(filter: &str) -> String {
    let stub = Stub::start(Plan {
        completions: vec![streamed_refusal()],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    let log = ego.home_path().join("diagnostics.log");
    let finished = ego
        .env("EGO_LOG", filter)
        .env("EGO_LOG_FILE", log.to_str().expect("UTF-8 temp path"))
        .args([
            "--sandbox",
            "off",
            "--model",
            "ollama/qwen3",
            "--retry",
            "0",
            "Ask",
        ])
        .run();
    assert_eq!(finished.code, Some(4), "{}", finished.stderr);
    std::fs::read_to_string(&log).unwrap_or_default()
}

// Catches: the capture target never receiving a streamed refusal, receiving it
// more than once, or writing an account value from the frame.
#[test]
fn a_named_capture_target_records_one_redacted_streamed_refusal() {
    let log = refused_with_log("ego::body::provider_failure=debug");
    assert_eq!(
        log.matches("an HTTP 200 stream ended in an error").count(),
        1,
        "{log}"
    );
    assert!(log.contains("status: 200"), "{log}");
    assert!(log.contains("overloaded_error"), "{log}");
    assert!(!log.contains("acct-private"), "{log}");
}

// Catches: a bare level an operator sets to watch something else writing raw
// provider frames into the log.
#[test]
fn a_bare_debug_level_does_not_record_provider_frames() {
    let log = refused_with_log("debug");
    assert!(!log.contains("ended in an error"), "{log}");
    assert!(!log.contains("overloaded_error"), "{log}");
}
