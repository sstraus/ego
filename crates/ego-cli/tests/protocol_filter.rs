//! Real executable + owned loopback provider; no paid endpoints or credentials.
#![cfg(unix)]
mod support;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::json;
use std::fs;
use support::{Ego, Plan, Reply, Stub};

fn fixture(ego: &Ego, config: serde_json::Value) -> std::path::PathBuf {
    let path = ego.home_path().join("protocol-filter");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ego-protocol-filter-fixture"), &path).unwrap();
    fs::write(
        path.with_extension("json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    path
}
fn configure(ego: &Ego, path: &std::path::Path, provider: &str, extra: &str) {
    ego.home_file(
        "config.toml",
        &format!(
            "[auth.{provider}.protocol_filter]\ncommand = '{}'\n{extra}\n",
            path.display()
        ),
    );
}
fn invocation(ego: Ego) -> Ego {
    invocation_prompt(ego, "hello filter")
}
fn invocation_prompt(ego: Ego, prompt: &str) -> Ego {
    ego.args([
        "--model",
        "anthropic/claude-sonnet-5-5",
        "--thinking",
        "low",
        "--sandbox",
        "off",
        "--output",
        "json",
        "--max-turns",
        "1",
        prompt,
    ])
}
const EXACT: &str = "{ \"model\":\"claude-sonnet-5-5\",\"messages\":[{\"role\":\"user\",\"content\":\"filtered\"}],\"max_tokens\":64,\"stream\":true }\n";

fn resume_filter(
    home: &std::path::Path,
    workspace: &std::path::Path,
    stub: &Stub,
    id: &str,
    command: &str,
) -> support::Finished {
    Ego::at(home, workspace, Some(stub))
        .env("ANTHROPIC_API_KEY", "filter-test-api-key")
        .subcommand(command)
        .arg(id)
        .args([
            "--thinking",
            "low",
            "--sandbox",
            "off",
            "--output",
            "json",
            "--max-turns",
            "1",
            "continue filter",
        ])
        .run()
}

// Catches: losing first-captured variables across subprocesses/resume, leaking
// another session/filter's values, or allowing a later invocation to replace them.
#[test]
fn protocol_filter_first_value_survives_resume_and_is_isolated_and_immutable() {
    let stub = Stub::start_concurrent(Plan::default());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let marker = home.path().join("state-contexts");
    let setup = Ego::at(home.path(), workspace.path(), Some(&stub));
    let path = fixture(
        &setup,
        json!({"version":1,"mode":"variables","marker":marker}),
    );
    configure(&setup, &path, "anthropic", "");
    let a = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718291";
    let b = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718292";
    std::thread::scope(|scope| {
        let run = |id| {
            invocation(
                Ego::at(home.path(), workspace.path(), Some(&stub))
                    .env("ANTHROPIC_API_KEY", "filter-test-api-key")
                    .args(["--session-id", id]),
            )
            .run()
        };
        let first = scope.spawn(move || run(a));
        let second = scope.spawn(move || run(b));
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_eq!(first.code, Some(0), "{first:?}");
        assert_eq!(second.code, Some(0), "{second:?}");
    });
    let resumed = resume_filter(home.path(), workspace.path(), &stub, a, "resume");
    assert_eq!(resumed.code, Some(0), "{resumed:?}");
    let continued = resume_filter(home.path(), workspace.path(), &stub, a, "continue");
    assert_eq!(continued.code, Some(0), "{continued:?}");
    let contexts: Vec<serde_json::Value> = fs::read_to_string(&marker)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(contexts.len(), 4);
    let first_a = contexts.iter().find(|v| v["session_id"] == a).unwrap();
    let first_b = contexts.iter().find(|v| v["session_id"] == b).unwrap();
    assert_eq!(first_a["state"]["values"], json!({}));
    assert_eq!(first_b["state"]["values"], json!({}));
    assert_eq!(first_a["turn_index"], 0);
    assert_eq!(first_b["turn_index"], 0);
    // Catches counting all prompt items, process invocations, or parent sessions.
    let a_indices: Vec<_> = contexts
        .iter()
        .filter(|value| value["session_id"] == a)
        .map(|value| value["turn_index"].as_u64().unwrap())
        .collect();
    assert_eq!(a_indices, [0, 1, 2]);
    assert_ne!(first_a["attempt_id"], first_b["attempt_id"]);
    let last = contexts.last().unwrap();
    assert_eq!(
        last["state"]["values"]["first_attempt"],
        first_a["attempt_id"]
    );
    assert_ne!(last["attempt_id"], first_a["attempt_id"]);
    let headers: Vec<_> = stub
        .requests()
        .into_iter()
        .filter(|r| r.path.ends_with("/messages"))
        .map(|r| r.headers["x-ego-filter"].clone())
        .collect();
    assert_eq!(
        headers
            .iter()
            .filter(|h| h.as_str() == first_a["attempt_id"].as_str().unwrap())
            .count(),
        3
    );
    assert_eq!(
        headers
            .iter()
            .filter(|h| h.as_str() == first_b["attempt_id"].as_str().unwrap())
            .count(),
        1
    );

    // Equal writes above did not advance the revision. A conflicting write
    // cannot replace the fact or reach the provider.
    assert_eq!(last["state"]["revision"], 1);
    fs::write(
        path.with_extension("json"),
        serde_json::to_vec(&json!({
            "version":1,"mode":"variables","replace":"changed-value"
        }))
        .unwrap(),
    )
    .unwrap();
    let refused = resume_filter(home.path(), workspace.path(), &stub, a, "resume");
    assert_ne!(refused.code, Some(0), "{refused:?}");
    assert_eq!(stub.generations().len(), 4);

    // A different configured executable gets a fresh namespace in the same session.
    let other = home.path().join("other-filter");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_ego-protocol-filter-fixture"), &other).unwrap();
    fs::write(
        other.with_extension("json"),
        serde_json::to_vec(&json!({"version":1,"mode":"variables","marker":marker})).unwrap(),
    )
    .unwrap();
    configure(&setup, &other, "anthropic", "");
    let changed = resume_filter(home.path(), workspace.path(), &stub, a, "resume");
    assert_eq!(changed.code, Some(0), "{changed:?}");
    let final_context: serde_json::Value =
        serde_json::from_str(fs::read_to_string(&marker).unwrap().lines().last().unwrap()).unwrap();
    assert_eq!(final_context["state"]["values"], json!({}));
    assert_eq!(final_context["state"]["revision"], 0);
    assert_eq!(final_context["turn_index"], 4);
}

// Catches a fixed one-second child timeout killing a healthy, slow hello exchange.
#[test]
fn protocol_filter_default_allows_delayed_healthy_child() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
    let path = fixture(&ego, json!({"mode":"pass", "hello_mode":"delayed-pass"}));
    configure(&ego, &path, "anthropic", "");
    let result = invocation(ego).run();
    assert_eq!(result.code, Some(0), "{result:?}");
    assert_eq!(stub.generations().len(), 1);
}

// Catches: sending uninterested request/response bytes to the executable,
// treating a malformed or timed-out declaration as pass-through, or launching
// a response exchange for a request-only filter.
#[test]
fn protocol_filter_startup_interest_selects_each_direction_and_fails_closed() {
    for (request_interest, response_interest) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let marker = ego.home_path().join("interest-requests");
        let response_marker = ego.home_path().join("interest-responses");
        let path = fixture(
            &ego,
            json!({"mode":"pass", "marker":marker, "response_marker":response_marker,
            "request_interest":request_interest,"response_interest":response_interest,
            "response_mode": if response_interest {"pass"} else {"fail"}}),
        );
        configure(&ego, &path, "anthropic", "");
        let result = invocation(ego).run();
        assert_eq!(
            result.code,
            Some(0),
            "{request_interest}/{response_interest}: {result:?}"
        );
        assert_eq!(stub.generations().len(), 1);
        assert_eq!(marker.exists(), request_interest);
        assert_eq!(response_marker.exists(), response_interest);
    }
    for hello_mode in ["invalid", "missing", "malformed", "sleep"] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let path = fixture(&ego, json!({"mode":"pass", "hello_mode":hello_mode}));
        configure(
            &ego,
            &path,
            "anthropic",
            if hello_mode == "sleep" {
                "timeout_ms = 50"
            } else {
                ""
            },
        );
        let result = invocation(ego).run();
        assert_ne!(result.code, Some(0), "{hello_mode}: {result:?}");
        assert_eq!(stub.generations().len(), 0, "{hello_mode}");
    }
}

// Catches: invoking the filter outside actual attempts, or bypassing the
// caller's explicit retry limit on a truncated exchange.
#[test]
fn protocol_filter_retry_attempts_match_actual_provider_deliveries() {
    for (first, retries, expected, success) in [
        // A lost stream, not a capacity refusal, so the retry waits a second.
        (Reply::TruncatedSse, "1", 2, true),
        (Reply::TruncatedSse, "0", 1, false),
    ] {
        let stub = Stub::start(Plan {
            completions: vec![first, Reply::Sse(support::text("recovered"))],
            ..Plan::default()
        });
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let marker = ego.home_path().join("attempts");
        let path = fixture(&ego, json!({"mode":"pass","marker":marker}));
        configure(&ego, &path, "anthropic", "");
        let result = invocation(ego.args(["--retry", retries])).run();
        assert_eq!(result.code == Some(0), success, "{result:?}");
        assert_eq!(stub.generations().len(), expected, "{result:?}");
        let attempts: Vec<serde_json::Value> = fs::read_to_string(marker)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(attempts.len(), expected);
        assert!(attempts.iter().all(|attempt| attempt["turn_index"] == 0));
        if expected == 2 {
            assert_eq!(attempts[0]["session_id"], attempts[1]["session_id"]);
            assert_eq!(attempts[0]["generation_id"], attempts[1]["generation_id"]);
            assert_ne!(attempts[0]["attempt_id"], attempts[1]["attempt_id"]);
        }
    }
}

// Catches: post-filter JSON serialization, lost authentication, inherited secrets, or preflight execution.
#[test]
fn protocol_filter_exact_bytes_auth_isolation_and_one_actual_invocation() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .env("ANTHROPIC_API_KEY", "filter-test-api-key")
        .env("UNRELATED_SECRET", "must-not-inherit");
    let marker = ego.home_path().join("filter-invocations");
    let capture = ego.home_path().join("filter-input");
    let output = json!({"version":1,"body_base64":STANDARD.encode(EXACT),"identity_headers":[{"name":"X-Ego-Filter","value":"v1"}],"expected_revision":0,"state_writes":{}});
    let path = fixture(
        &ego,
        json!({"mode":"raw","raw":output.to_string(),"marker":marker,"capture":capture}),
    );
    configure(&ego, &path, "anthropic", "");
    let result = invocation(ego).run();
    assert_eq!(
        result.code,
        Some(0),
        "{result:?}; filter diagnostics: {:?}",
        result
            .durable_payloads()
            .into_iter()
            .filter(|v| v.to_string().contains("protocol_filter"))
            .collect::<Vec<_>>()
    );
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 1);
    let input: serde_json::Value = serde_json::from_slice(&fs::read(capture).unwrap()).unwrap();
    let body = STANDARD
        .decode(input["body_base64"].as_str().unwrap())
        .unwrap();
    assert!(
        !String::from_utf8(body)
            .unwrap()
            .contains("filter-test-api-key")
    );
    let requests = stub.requests();
    let sent = requests
        .iter()
        .find(|r| r.path.ends_with("/messages"))
        .unwrap();
    assert_eq!(sent.body.as_bytes(), EXACT.as_bytes());
    assert_eq!(
        sent.headers.get("x-ego-filter").map(String::as_str),
        Some("v1")
    );
    assert_eq!(
        sent.headers.get("x-api-key").map(String::as_str),
        Some("filter-test-api-key")
    );
    assert_eq!(
        sent.headers
            .get("content-length")
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        EXACT.len()
    );
    assert!(!result.stderr.contains("filter-test-api-key"));
}

// Catches: refusing beta or routing headers emitted by the selected filter child.
#[test]
fn protocol_filter_beta_and_routing_headers_reach_provider() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
    let output = json!({"version":1,"body_base64":STANDARD.encode(EXACT),
        "identity_headers":[{"name":"anthropic-beta","value":"feature-test-v1"},
            {"name":"x-forwarded-host","value":"route-test.example"}],
        "expected_revision":0,"state_writes":{}});
    let path = fixture(&ego, json!({"mode":"raw","raw":output.to_string()}));
    ego.home_file(
        "config.toml",
        &format!(
            "[auth.anthropic.protocol_filter]\ncommand = '{}'\n",
            path.display()
        ),
    );
    let result = invocation(ego).run();
    assert_eq!(result.code, Some(0), "{result:?}");
    let requests = stub.requests();
    let sent = requests
        .iter()
        .find(|r| r.path.ends_with("/messages"))
        .unwrap();
    assert_eq!(
        sent.headers.get("anthropic-beta").map(String::as_str),
        Some("feature-test-v1")
    );
    assert_eq!(
        sent.headers.get("x-forwarded-host").map(String::as_str),
        Some("route-test.example")
    );
    assert_eq!(
        sent.headers.get("x-api-key").map(String::as_str),
        Some("filter-test-api-key")
    );
}

// Catches: retaining the original credential after the operator's filter
// deliberately replaced it with a different value.
#[test]
fn protocol_filter_can_replace_an_existing_credential() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
    let output = json!({"version":1,"body_base64":STANDARD.encode(EXACT),
        "identity_headers":[{"name":"x-api-key","value":"replacement-test-key"}],
        "expected_revision":0,"state_writes":{}});
    let path = fixture(&ego, json!({"mode":"raw","raw":output.to_string()}));
    configure(&ego, &path, "anthropic", "");
    let result = invocation(ego).run();
    assert_eq!(result.code, Some(0), "{result:?}");
    let requests = stub.requests();
    let sent = requests
        .iter()
        .find(|r| r.path.ends_with("/messages"))
        .unwrap();
    assert_eq!(
        sent.headers.get("x-api-key").map(String::as_str),
        Some("replacement-test-key")
    );
    assert!(!result.stderr.contains("filter-test-api-key"));
}

// Catches: global execution of a provider-scoped filter, or ignoring enabled=false.
#[test]
fn protocol_filter_disabled_and_other_provider_preserve_ordinary_request() {
    for (provider, extra) in [("anthropic", "enabled = false"), ("openai", "")] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let path = fixture(&ego, json!({"mode":"exit"}));
        configure(&ego, &path, provider, extra);
        let result = invocation(ego).run();
        assert_eq!(result.code, Some(0), "{provider}: {result:?}");
        let requests = stub.requests();
        let sent = requests
            .iter()
            .find(|r| r.path.ends_with("/messages"))
            .unwrap();
        assert!(sent.body.contains("hello filter"));
        assert!(!sent.headers.contains_key("x-ego-filter"));
    }
}

// Catches: unsafe fallback after command errors or acceptance of an unversioned/ambiguous exchange.
#[test]
fn protocol_filter_invalid_exchanges_fail_before_provider_delivery() {
    let valid = |body: &str, headers: serde_json::Value| {
        json!({"version":1,"body_base64":body,"identity_headers":headers,"expected_revision":0,"state_writes":{}}).to_string()
    };
    let cases = vec![
        ("malformed", "not JSON".to_owned()),
        (
            "missing version",
            r#"{"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{}}"#.into(),
        ),
        (
            "wrong version",
            r#"{"version":2,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{}}"#.into(),
        ),
        (
            "unknown field",
            r#"{"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{},"extra":1}"#.into(),
        ),
        (
            "duplicate field",
            r#"{"version":1,"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{}}"#.into(),
        ),
        (
            "trailing object",
            r#"{"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{}}{}"#.into(),
        ),
        (
            "duplicate variable",
            r#"{"version":1,"body_base64":"e30=","identity_headers":[],"expected_revision":0,"state_writes":{"first":"a","first":"b"}}"#.into(),
        ),
        ("invalid base64", valid("!", json!([]))),
        ("empty body", valid("", json!([]))),
        ("exit", String::new()),
        ("sleep", String::new()),
        ("oversize envelope", String::new()),
        ("oversize decoded body", String::new()),
        (
            "duplicate header",
            valid(
                "e30=",
                json!([{"name":"x-ego-filter","value":"a"},{"name":"X-Ego-Filter","value":"b"}]),
            ),
        ),
        (
            "newline",
            valid("e30=", json!([{"name":"x-ego-filter","value":"a\r\nb"}])),
        ),
    ];
    let mut failures = Vec::new();
    for (name, code) in cases {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let mode = if matches!(
            name,
            "exit" | "sleep" | "oversize envelope" | "oversize decoded body"
        ) {
            name
        } else {
            "raw"
        };
        let path = fixture(&ego, json!({"mode":mode,"raw":code}));
        configure(
            &ego,
            &path,
            "anthropic",
            if name == "sleep" {
                "timeout_ms = 50"
            } else {
                ""
            },
        );
        let result = invocation(ego).run();
        if result.code == Some(0)
            || !stub.generations().is_empty()
            || !result.stderr.contains("protocol_filter")
            || result.stderr.contains("private diagnostic")
            || (name == "exit"
                && !result.stderr.contains(
                    "protocol_filter request: exit (check why the filter exits unsuccessfully)",
                ))
        {
            failures.push(format!(
                "{name}: {result:?}; deliveries={}",
                stub.generations().len()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Catches: an active filter ignoring run cancellation and dispatching its output afterward.
#[test]
fn protocol_filter_active_cancellation_has_no_delivery() {
    use std::time::{Duration, Instant};
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
    let marker = ego.home_path().join("active-filter");
    let hello_marker = ego.home_path().join("active-filter-hello");
    let path = fixture(
        &ego,
        json!({"mode":"active","marker":marker,"hello_marker":hello_marker}),
    );
    configure(&ego, &path, "anthropic", "");
    let running = invocation(ego).spawn();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !marker.exists() {
        panic!(
            "generation never entered the filter (hello received: {}): {:?}",
            hello_marker.exists(),
            running.finish()
        );
    }
    let signal = std::process::Command::new("/bin/kill")
        .args(["-INT", &running.child.id().to_string()])
        .output()
        .unwrap();
    assert!(signal.status.success());
    let result = running.finish();
    assert_eq!(result.code, Some(130), "{result:?}");
    assert!(stub.generations().is_empty());
}

// Catches: reused global context crossing simultaneous sessions or a resumed attempt receiving a new session identity.
#[test]
fn protocol_filter_concurrent_sessions_and_resume_keep_attempt_context_separate() {
    let stub = Stub::start_concurrent(Plan::default());
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let marker = home.path().join("contexts");
    let setup = Ego::at(home.path(), workspace.path(), Some(&stub));
    let path = fixture(&setup, json!({"mode":"pass","marker":marker}));
    configure(&setup, &path, "anthropic", "");
    let a = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718293";
    let b = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718294";
    std::thread::scope(|scope| {
        let run = |id| {
            invocation(
                Ego::at(home.path(), workspace.path(), Some(&stub))
                    .env("ANTHROPIC_API_KEY", "filter-test-api-key")
                    .args(["--session-id", id]),
            )
            .run()
        };
        let first = scope.spawn(move || run(a));
        let second = scope.spawn(move || run(b));
        assert_eq!(first.join().unwrap().code, Some(0));
        assert_eq!(second.join().unwrap().code, Some(0));
    });
    let resumed = Ego::at(home.path(), workspace.path(), Some(&stub))
        .env("ANTHROPIC_API_KEY", "filter-test-api-key")
        .subcommand("resume")
        .arg(a)
        .args([
            "--thinking",
            "low",
            "--sandbox",
            "off",
            "--output",
            "json",
            "--max-turns",
            "1",
            "continue filter",
        ])
        .run();
    assert_eq!(resumed.code, Some(0), "{resumed:?}");
    let contexts: Vec<serde_json::Value> = fs::read_to_string(marker)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(contexts.len(), 3);
    assert_eq!(contexts.iter().filter(|v| v["session_id"] == a).count(), 2);
    assert_eq!(contexts.iter().filter(|v| v["session_id"] == b).count(), 1);
    let attempts: std::collections::HashSet<_> = contexts
        .iter()
        .map(|v| v["attempt_id"].as_str().unwrap())
        .collect();
    assert_eq!(attempts.len(), 3);
}

// Catches: sending the original secret to HTTP, losing the mapping on resume,
// or displaying provider placeholders instead of restoring local values.
#[test]
fn protocol_filter_dlp_round_trip_masks_provider_and_restores_after_resume() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::EchoUserTextContaining(
            "Return this exact text:".to_owned(),
        )],
        ..Plan::default()
    });
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let setup = Ego::at(home.path(), workspace.path(), Some(&stub));
    let filter = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/protocol-filter/dlp.py")
        .canonicalize()
        .unwrap();
    configure(&setup, &filter, "anthropic", "");
    let id = "0198c7f4-9c1f-7a2b-8c3d-4e5f60718290";
    let secret = "-----BEGIN PRIVATE KEY-----\nSYNTHETIC-TEST-MATERIAL\n-----END PRIVATE KEY-----";
    let prompt = format!("Return this exact text: {secret} and owner@example.test");
    let first = invocation_prompt(
        Ego::at(home.path(), workspace.path(), Some(&stub))
            .env("ANTHROPIC_API_KEY", "filter-test-api-key")
            .args(["--session-id", id]),
        &prompt,
    )
    .run();
    assert_eq!(first.code, Some(0), "{first:?}");
    assert!(
        first.stdout.contains("SYNTHETIC-TEST-MATERIAL"),
        "{first:?}"
    );
    assert!(first.stdout.contains("owner@example.test"), "{first:?}");
    assert!(!first.stdout.contains("__EGO_DLP_"), "{first:?}");
    let resumed = Ego::at(home.path(), workspace.path(), Some(&stub))
        .env("ANTHROPIC_API_KEY", "filter-test-api-key")
        .subcommand("resume")
        .arg(id)
        .args([
            "--thinking",
            "low",
            "--sandbox",
            "off",
            "--output",
            "json",
            "--max-turns",
            "1",
            &prompt,
        ])
        .run();
    assert_eq!(resumed.code, Some(0), "{resumed:?}");
    assert!(
        resumed.stdout.contains("SYNTHETIC-TEST-MATERIAL"),
        "{resumed:?}"
    );
    let requests: Vec<_> = stub
        .requests()
        .into_iter()
        .filter(|r| r.path.ends_with("/messages"))
        .collect();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(!request.body.contains("SYNTHETIC-TEST-MATERIAL"));
        assert!(!request.body.contains("owner@example.test"));
        assert!(request.body.contains("__EGO_DLP_"));
    }
    let placeholders = |body: &str| {
        body.split("__EGO_DLP_")
            .skip(1)
            .map(|tail| tail[..32].to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        placeholders(&requests[0].body),
        placeholders(&requests[1].body)
    );
}

// Catches: early EOF and an oversized result collapsing to one diagnosis,
// or the diagnostic exposing child stderr in either output or the journal.
#[test]
fn protocol_filter_request_early_eof_is_distinct_from_output_limit() {
    for (mode, category) in [
        ("early eof", "early EOF"),
        ("oversize envelope", "output limit"),
    ] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let path = fixture(&ego, json!({"mode":mode}));
        configure(&ego, &path, "anthropic", "");
        let result = invocation(ego).run();
        assert_eq!(result.code, Some(2), "{mode}: {result:?}");
        assert!(stub.generations().is_empty(), "{mode}: provider dispatched");
        let expected = format!("protocol_filter request: {category} (");
        assert!(result.stderr.contains(&expected), "{mode}: {result:?}");
        let journal = result.durable_payloads();
        assert!(
            journal.iter().any(|record| {
                let event = &record["event"]["kind"];
                event["kind"] == "Error"
                    && event["message"].as_str().is_some_and(|message| {
                        message.starts_with(&expected) && result.stderr.contains(message)
                    })
            }),
            "{mode}: no matching durable failure"
        );
        for diagnostic in [&result.stderr, &result.stdout, &format!("{journal:?}")] {
            assert!(!diagnostic.contains("private stderr sentinel"), "{mode}");
        }
    }
}

// Catches: forwarding an unsupported non-JSON provider error without
// verifying that the response filter can safely restore its content.
#[test]
fn protocol_filter_rejects_non_json_error_without_unfiltered_fallback() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400)],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
    let filter = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/protocol-filter/dlp.py")
        .canonicalize()
        .unwrap();
    configure(&ego, &filter, "anthropic", "");
    let result = invocation(ego).run();
    assert_eq!(stub.generations().len(), 1, "{result:?}");
    assert_eq!(result.code, Some(2), "{result:?}");
    assert!(
        result.stderr.contains("protocol_filter response:"),
        "{result:?}"
    );
    assert!(!result.stderr.contains("the stub was told"), "{result:?}");
}

// Catches: inbound filter failures treated as successful provider completions
// or retried as provider-transient errors after the request was dispatched.
#[test]
fn protocol_filter_response_failures_are_local_and_never_fall_back() {
    for mode in ["fail", "bad_sequence"] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let path = fixture(&ego, json!({"mode":"pass","response_mode":mode}));
        configure(&ego, &path, "anthropic", "");
        let result = invocation(ego).run();
        assert_ne!(result.code, Some(0), "{mode}: {result:?}");
        assert_eq!(stub.generations().len(), 1, "{mode}: {result:?}");
        assert!(
            result.stderr.contains("protocol_filter"),
            "{mode}: {result:?}"
        );
    }
}

// Catches: composing two filters, ignoring the global fallback, or treating a
// disabled provider override as permission to execute the global command.
#[test]
fn protocol_filter_global_fallback_provider_override_and_opt_out_select_one_process() {
    for selection in ["global", "override", "disabled", "global-disabled"] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let capture = ego.home_path().join("hello-capture");
        let path = fixture(&ego, json!({"mode":"pass","hello_capture":capture}));
        let absent = ego.home_path().join("must-not-execute");
        let config = match selection {
            "global" => format!("[protocol_filter]\ncommand='{}'\n", path.display()),
            "override" => format!(
                "[protocol_filter]\ncommand='{}'\n[auth.anthropic.protocol_filter]\ncommand='{}'\n",
                absent.display(),
                path.display()
            ),
            "disabled" => format!(
                "[protocol_filter]\ncommand='{}'\n[auth.anthropic.protocol_filter]\ncommand='{}'\nenabled=false\n",
                absent.display(),
                path.display()
            ),
            _ => format!(
                "[protocol_filter]\ncommand='{}'\nenabled=false\n",
                path.display()
            ),
        };
        ego.home_file("config.toml", &config);
        let result = invocation(ego).run();
        assert_eq!(result.code, Some(0), "{selection}: {result:?}");
        assert_eq!(stub.generations().len(), 1, "{selection}");
        if matches!(selection, "global" | "override") {
            let records = fs::read_to_string(&capture).unwrap();
            assert_eq!(records.lines().count(), 1, "{selection}");
            let hello: serde_json::Value = serde_json::from_str(records.trim()).unwrap();
            assert_eq!(hello["generation_role"], "main");
            assert_eq!(hello["wire_format"], "AnthropicMessages");
        } else {
            assert!(!capture.exists(), "{selection}");
        }
    }
}

// Catches: filter diagnostics entering model text, terminal escape injection,
// unbounded notices, and retry/bypass after a policy rejection at any phase.
#[test]
fn protocol_filter_notices_are_local_and_policy_rejection_never_retries() {
    for phase in ["notice", "hello", "request", "response", "notice-overflow"] {
        let stub = Stub::start(Plan::default());
        let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
        let capture = ego.home_path().join("hello-capture");
        let notice = "private diagnostic\n\u{1b}[31m\u{202e}local";
        let mut config =
            json!({"mode":"pass", "hello_capture":capture, "notices":[notice, "é".repeat(200)]});
        match phase {
            "hello" => config["hello_mode"] = json!("reject"),
            "request" => config["mode"] = json!("reject"),
            "response" => config["response_mode"] = json!("reject"),
            "notice-overflow" => config["notices"] = json!(["x", "x", "x", "x", "x"]),
            _ => {
                config["hello_notices"] = json!(["startup-one", "startup-two"]);
                config["response_notices"] = json!(["must-not-display", "must-not-display"]);
            }
        }
        let path = fixture(&ego, config);
        configure(&ego, &path, "anthropic", "");
        let result = invocation(ego)
            .args(["--fallback", "anthropic/claude-opus-5-5"])
            .run();
        assert_eq!(
            fs::read_to_string(&capture).unwrap().lines().count(),
            1,
            "{phase}"
        );
        assert!(
            !result.stdout.contains("private diagnostic"),
            "{phase}: {result:?}"
        );
        if phase == "notice" {
            assert_eq!(result.code, Some(0), "{result:?}");
            assert_eq!(
                result.stderr.matches("εgo filter:").count(),
                4,
                "{result:?}"
            );
            assert!(!result.stderr.contains("must-not-display"));
        } else {
            assert_ne!(result.code, Some(0), "{phase}: {result:?}");
        }
        if phase != "notice-overflow" {
            assert!(
                result
                    .stderr
                    .contains("εgo filter: private diagnostic[31mlocal"),
                "{phase}: {result:?}"
            );
            assert!(
                result
                    .stderr
                    .contains(&format!("εgo filter: {}\n", "é".repeat(128))),
                "{result:?}"
            );
            assert!(!result.stderr.contains(&"é".repeat(129)));
            assert!(!result.stderr.contains('\u{202e}'));
            assert!(!result.stderr.contains("\u{1b}[31m"));
        } else {
            assert!(!result.stderr.contains("εgo filter:"));
        }
        assert_eq!(
            stub.generations().len(),
            usize::from(matches!(phase, "notice" | "response")),
            "{phase}: {result:?}"
        );
        if matches!(phase, "hello" | "request" | "response") {
            assert!(
                format!("{}{}", result.stdout, result.stderr)
                    .contains("policy rejected this attempt"),
                "{phase}: {result:?}"
            );
        }
    }
}

struct EgressFixtureResult {
    finished: support::Finished,
    requests: Vec<support::Recorded>,
    envelopes: Vec<serde_json::Value>,
    restored: bool,
}

fn run_egress_fixture(destination: &str, mode: &str, restore: bool) -> EgressFixtureResult {
    let secret = "restored-egress-secret-1030";
    let content = if restore { "RESTORE_SECRET" } else { "public" };
    let reply = if destination == "mcp" {
        support::tool_call(
            "egress-call",
            "search_tools",
            &json!({"query":content}).to_string(),
        )
    } else {
        support::text(content)
    };
    let stub = Stub::start_mcp_egress(Plan {
        completions: vec![Reply::Sse(reply), Reply::Sse(support::text("done"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).env("ANTHROPIC_API_KEY", "filter-test-api-key");
    let restored = ego.home_path().join("restored");
    let capture = ego.home_path().join("egress");
    let filter = fixture(
        &ego,
        json!({"version":1,"mode":"egress","secret":secret,
        "restored_marker":restored,"egress_capture":capture,"egress_mode":mode}),
    );
    let server = format!(
        "[mcp.servers.remote]\ntransport='http'\nurl='http://{}/mcp'\nauth='none'\nallow_insecure_loopback=true\n",
        stub.address
    );
    let remote = match destination {
        "mcp" => server,
        "mcp_hook" => format!(
            "{server}\n[[hooks]]\nevent='Stop'\n[hooks.handler]\ntype='mcp_tool'\nserver='remote'\ntool='search_tools'\ninput={{query='public'}}\n"
        ),
        "http_hook" => format!(
            "[[hooks]]\nevent='Stop'\n[hooks.handler]\ntype='http'\nurl='http://{}/hook'\n",
            stub.address
        ),
        _ => unreachable!(),
    };
    // Hook execution never settles an opaque-tool permission prompt. Admit
    // the fixture's exact tool and loopback route before testing its payload.
    let permissions = format!(
        "[permissions]\nallow=['tool(remote/search_tools)','net({})']\n",
        stub.address
    );
    ego.home_file(
        "config.toml",
        &format!(
            "[protocol_filter]\ncommand='{}'\ntimeout_ms=500\n{permissions}{remote}",
            filter.display()
        ),
    );
    let finished = invocation(ego.args(["--mode", "yolo"])).run();
    let envelopes = fs::read_to_string(&capture)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    EgressFixtureResult {
        finished,
        requests: stub.requests(),
        envelopes,
        restored: fs::read_to_string(&restored).ok().as_deref() == Some(secret),
    }
}

fn selected_egress_requests(
    result: &EgressFixtureResult,
    destination: &str,
) -> Vec<serde_json::Value> {
    result
        .requests
        .iter()
        .filter_map(|request| {
            let body: serde_json::Value = serde_json::from_str(&request.body).ok()?;
            if destination == "http_hook" {
                (request.path == "/hook").then_some(body)
            } else {
                (body["method"] == "tools/call").then_some(body)
            }
        })
        .collect()
}

// Catches: restored provider secrets bypassing the filter at remote MCP and HTTP hooks.
#[test]
fn protocol_filter_egress_blocks_restored_secrets_at_remote_dispatch() {
    let mut failures = Vec::new();
    for destination in ["mcp", "http_hook", "mcp_hook"] {
        let result = run_egress_fixture(destination, "reject", true);
        if !result.restored {
            failures.push(format!(
                "restoration missing at {destination}: {:?}",
                result.finished
            ));
        }
        if !selected_egress_requests(&result, destination).is_empty() {
            failures.push(format!("clear restored secret reached {destination}"));
        }
        if result.envelopes.is_empty() {
            failures.push(format!("filter bypassed at {destination}"));
        }
        if !result
            .finished
            .stderr
            .contains("secret blocked at external dispatch")
        {
            failures.push(format!(
                "filter notice lost at {destination}: {:?}",
                result.finished
            ));
        }
        for frame in &result.envelopes {
            if frame["state"]["values"]["restored_secret"] != "restored-egress-secret-1030" {
                failures.push(format!("provider mapping missing at {destination}"));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

// Catches: malformed output, state/header writes or MCP/hook payload rewrites
// escaping fail-closed handling; egress filters accept or reject only.
#[test]
fn protocol_filter_egress_invalid_output_and_rewrites_never_deliver() {
    let mut failures = Vec::new();
    for destination in ["mcp", "http_hook", "mcp_hook"] {
        for mode in [
            "invalid",
            "writes",
            "headers",
            "timeout",
            "transform",
            "pass",
        ] {
            let result = run_egress_fixture(destination, mode, true);
            if !result.restored || result.envelopes.is_empty() {
                failures.push(format!(
                    "fixture failed: {destination}/{mode}: {:?}",
                    result.finished
                ));
                continue;
            }
            let delivered = selected_egress_requests(&result, destination);
            let allowed = mode == "pass";
            if allowed != !delivered.is_empty() {
                failures.push(format!(
                    "delivery mismatch: {destination}/{mode}: {delivered:?}; {:?}",
                    result.finished
                ));
            }
        }
        // A no-secret request must still reach its intended destination.
        let public = run_egress_fixture(destination, "reject", false);
        if public.envelopes.is_empty() || selected_egress_requests(&public, destination).len() != 1
        {
            failures.push(format!(
                "public request refused at {destination}: {:?}",
                public.finished
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
