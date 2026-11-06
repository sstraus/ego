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
    ego.home_file("config.toml", &format!(
        "[auth.{provider}.protocol_filter]\ncommand = '{}'\nidentity_headers = ['x-ego-filter']\n{extra}\n", path.display()));
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
            "forbidden",
            valid("e30=", json!([{"name":"Authorization","value":"evil"}])),
        ),
        (
            "undeclared",
            valid("e30=", json!([{"name":"x-other","value":"evil"}])),
        ),
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
    let path = fixture(&ego, json!({"mode":"active","marker":marker}));
    configure(&ego, &path, "anthropic", "");
    let running = invocation(ego).spawn();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !marker.exists() {
        panic!(
            "generation never entered the filter: {:?}",
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
