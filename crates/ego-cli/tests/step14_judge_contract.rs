//! contract for the separately resolved, tool-less `auto` judge.
//!
//! These tests run the real `ego` binary against the existing provider stub.
//! The first generation asks for the argv-native `exec` tool; the second is the
//! judge response. This keeps the judge path a production TurnLoop path and
//! proves that a test-only approver cannot make the contract pass.

mod support;

use std::path::Path;

use support::{Ego, Plan, Reply, Stub, text, tool_call};

const MODEL: &str = "ollama/qwen3";
const HOSTED_MODEL: &str = "anthropic/claude-test-5";
const XAI_JUDGE_MODEL: &str = "xai-oauth/grok-code-fast-1";

const ANTHROPIC_FACTS: &str = r#"
[model."anthropic/claude-test-5".capabilities]
schema_version = 5
max_context_tokens = 400000
max_output_tokens = 128000
max_tools = 128
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "long"
transport = "sse"

[model."anthropic/claude-test-5".pricing]
revision = "anthropic/2026-08-01"
currency = "USD"
uncached_input = "0.00000125"
cache_read = "0.000000125"
cache_write = "0.0000015625"
output = "0.00001"
"#;

const XAI_FACTS: &str = r#"
[model."xai-oauth/grok-code-fast-1".capabilities]
schema_version = 5
max_context_tokens = 400000
max_output_tokens = 128000
max_tools = 128
supports_parallel_tool_calls = true
reasoning_carryover = "same_provider_only"
reasoning_efforts = "any"
default_reasoning_effort = "off"
cancel = "cooperative"
cache = "long"
transport = "sse"
"#;

fn judge_response(verdict: &str, reason: &str) -> Reply {
    let body = serde_json::json!({
        "verdict": verdict,
        "reason": reason,
    })
    .to_string();
    Reply::Sse(text(&body))
}

fn exec_request() -> Reply {
    Reply::Sse(tool_call(
        "judge-exec",
        "exec",
        r#"{"argv":["/bin/echo","judge exact argv"],"cwd":null,"timeout_ms":1000}"#,
    ))
}

fn marker_exec_request(marker: &Path) -> Reply {
    let marker = marker.to_str().expect("temporary marker path is UTF-8");
    let arguments = serde_json::json!({
        "argv": [
            "/bin/sh",
            "-c",
            "printf judge-executed > \"$1\"",
            "ego-judge-marker",
            marker,
        ],
        "cwd": null,
        "timeout_ms": 1000,
    })
    .to_string();
    Reply::Sse(tool_call("judge-exec-marker", "exec", &arguments))
}

fn plan(judge: Reply, final_answer: Option<Reply>) -> Plan {
    let mut completions = vec![exec_request(), judge];
    if let Some(final_answer) = final_answer {
        completions.push(final_answer);
    }
    Plan {
        completions,
        ..Plan::default()
    }
}

fn marker_plan(marker: &Path, judge: Reply, final_answer: Option<Reply>) -> Plan {
    let mut completions = vec![marker_exec_request(marker), judge];
    if let Some(final_answer) = final_answer {
        completions.push(final_answer);
    }
    Plan {
        completions,
        ..Plan::default()
    }
}

fn auto_run(stub: &Stub) -> Ego {
    let ego = Ego::against(stub);
    ego.home_file("config.toml", "[permissions]\njudge = \"ollama/qwen3\"\n");
    ego.args([
        "--model",
        MODEL,
        "--mode",
        "auto",
        "--sandbox",
        "off",
        "--output",
        "json",
        "Ask",
    ])
}

fn auto_args(ego: Ego) -> Ego {
    ego.args([
        "--model",
        MODEL,
        "--mode",
        "auto",
        "--sandbox",
        "off",
        "--output",
        "json",
        "Ask",
    ])
}

fn hosted_auto_run(stub: &Stub) -> Ego {
    let ego = Ego::against(stub);
    ego.home_file(
        "config.toml",
        "[permissions]\njudge = \"anthropic/claude-test-5\"\n",
    );
    ego.home_file("models.toml", ANTHROPIC_FACTS);
    ego.env("ANTHROPIC_API_KEY", "sk-test").args([
        "--model",
        HOSTED_MODEL,
        "--mode",
        "auto",
        "--sandbox",
        "off",
        "--budget",
        "1.905$",
        "--output",
        "json",
        "Ask",
    ])
}

fn request_json(stub: &Stub, index: usize) -> serde_json::Value {
    stub.generations()
        .into_iter()
        .nth(index)
        .unwrap_or_else(|| panic!("missing generation request {index}"))
}

fn durable_events(config: &Path) -> Vec<serde_json::Value> {
    let sessions = config
        .parent()
        .expect("config has a home directory")
        .join("sessions");
    let session = std::fs::read_dir(sessions)
        .expect("durable auto mode creates a session store")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("events.jsonl"))
        .find(|path| path.is_file())
        .expect("the auto run writes events.jsonl");
    std::fs::read_to_string(session)
        .expect("read durable events")
        .lines()
        .filter_map(|line| {
            let record: serde_json::Value =
                serde_json::from_str(line).expect("durable event is JSON");
            record
                .get("payload")
                .and_then(|payload| payload.get("event"))
                .cloned()
        })
        .collect()
}

fn assert_judge_decision(
    events: &[serde_json::Value],
    expected_allowed: Option<bool>,
    expected_model: &str,
    expected_roles: &[&str],
) {
    let roles: Vec<&str> = events
        .iter()
        .filter(|event| event["kind"]["kind"] == "GenerationStarted")
        .map(|event| {
            event["kind"]["role"]
                .as_str()
                .expect("generation role is durable")
        })
        .collect();
    assert_eq!(roles, expected_roles, "main/judge generation roles changed");

    let judge_generation = events
        .iter()
        .find(|event| {
            event["kind"]["kind"] == "GenerationStarted" && event["kind"]["role"] == "judge"
        })
        .expect("judge generation is durable");
    let decision = events
        .iter()
        .find(|event| event["kind"]["kind"] == "PermissionDecided")
        .expect("judge decision is durable");
    assert_eq!(
        decision["kind"]["allowed"],
        serde_json::json!(expected_allowed)
    );
    assert_eq!(decision["kind"]["by"]["by"], "judge");
    assert_eq!(decision["kind"]["by"]["model"], expected_model);
    assert_eq!(
        decision["kind"]["by"]["attempt"], judge_generation["kind"]["attempt"],
        "permission authority must name the deciding judge attempt"
    );
}

#[test]
fn auto_allow_uses_one_toolless_judge_and_then_executes_the_exact_call() {
    let stub = Stub::start(plan(
        judge_response("allow", "the demonstrated off policy is explicit"),
        Some(Reply::Sse(text("done"))),
    ));
    let ego = auto_run(&stub);
    let config = ego.home_file("config.toml", "[permissions]\njudge = \"ollama/qwen3\"\n");
    let finished = ego.run();

    assert_eq!(finished.code, Some(0), "stderr: {}", finished.stderr);
    let requests = stub.generations();
    assert_eq!(
        requests.len(),
        3,
        "one main, one judge, one final main generation"
    );
    let judge = request_json(&stub, 1);
    assert!(
        judge
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "judge request must be tool-less: {judge}"
    );
    let encoded = judge.to_string();
    assert!(
        !encoded.contains("Ask"),
        "judge received conversation prompt: {encoded}"
    );
    assert!(
        encoded.contains("exec"),
        "judge did not receive canonical call: {encoded}"
    );
    assert!(
        encoded.contains("off"),
        "judge did not receive effective confinement: {encoded}"
    );
    assert!(
        finished.stdout.contains("\"ok\":true"),
        "the already prepared call was not executed: {}",
        finished.stdout
    );
    assert_judge_decision(
        &durable_events(&config),
        Some(true),
        MODEL,
        &["main", "judge", "main"],
    );
}

#[test]
fn auto_uses_a_separately_resolved_judge_model_without_inheriting_main_tools() {
    let stub = Stub::start(plan(judge_response("deny", "judge model"), None));
    let ego = Ego::against(&stub);
    let config = ego.home_file(
        "config.toml",
        "[permissions]\njudge = \"anthropic/claude-test-5\"\n",
    );
    ego.home_file("models.toml", ANTHROPIC_FACTS);
    let finished = ego
        .env("ANTHROPIC_API_KEY", "sk-test")
        .args([
            "--model",
            MODEL,
            "--mode",
            "auto",
            "--sandbox",
            "off",
            "--output",
            "json",
            "Ask",
        ])
        .run();

    assert_eq!(
        finished.code,
        Some(3),
        "judge denial did not route to permission"
    );
    let requests = stub.generations();
    assert_eq!(requests.len(), 2, "main and judge each make one generation");
    assert_eq!(
        stub.requests()
            .iter()
            .filter(|request| !request.path.starts_with("/api/"))
            .count(),
        2,
        "static judge preflight must perform no HTTP beyond the main and judge generations"
    );
    assert_eq!(requests[0]["model"], "qwen3");
    assert_eq!(requests[1]["model"], "claude-test-5");
    assert!(
        requests[1]
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
        "judge request must be tool-less: {}",
        requests[1]
    );
    assert_judge_decision(
        &durable_events(&config),
        Some(false),
        HOSTED_MODEL,
        &["main", "judge"],
    );
}

#[test]
fn incompatible_judge_capabilities_refuse_before_generation_or_session_creation() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let config = ego.home_file(
        "config.toml",
        "[permissions]\njudge = \"anthropic/claude-test-5\"\n",
    );
    let incompatible_facts = ANTHROPIC_FACTS.replace(
        "reasoning_efforts = \"any\"\ndefault_reasoning_effort = \"off\"",
        "reasoning_efforts = \"unknown\"",
    );
    ego.home_file("models.toml", &incompatible_facts);

    let finished = ego
        .env("ANTHROPIC_API_KEY", "sk-test")
        .args([
            "--model",
            MODEL,
            "--mode",
            "auto",
            "--sandbox",
            "off",
            "--output",
            "json",
            "Ask",
        ])
        .run();

    assert_eq!(
        finished.code,
        Some(2),
        "incompatible judge was not a usage error"
    );
    assert!(
        finished.stderr.contains("incompatible") && finished.stderr.contains(HOSTED_MODEL),
        "the preflight refusal did not name the judge model: {}",
        finished.stderr
    );
    assert_eq!(
        stub.generations().len(),
        0,
        "an incompatible judge must not make a provider generation"
    );
    let sessions = config
        .parent()
        .expect("config has a home directory")
        .join("sessions");
    assert!(
        !sessions.exists()
            || std::fs::read_dir(&sessions)
                .expect("the sessions directory is readable")
                .next()
                .is_none(),
        "judge preflight must not create a durable session: {}",
        sessions.display()
    );
}

#[test]
fn inexpressible_judge_wire_refuses_before_generation_or_session_creation() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub);
    let config = ego.home_file(
        "config.toml",
        &format!("[permissions]\njudge = \"{XAI_JUDGE_MODEL}\"\n"),
    );
    ego.home_file("models.toml", XAI_FACTS);
    ego.home_file(
        "auth.json",
        r#"{"xai-oauth":{"type":"o_auth","access":"access-token"}}"#,
    );

    let finished = ego
        .args([
            "--model",
            MODEL,
            "--mode",
            "auto",
            "--sandbox",
            "off",
            "--output",
            "json",
            "Ask",
        ])
        .run();

    assert_eq!(
        finished.code,
        Some(2),
        "an inexpressible judge request was not a startup usage error; stderr={:?}; stdout={:?}",
        finished.stderr,
        finished.stdout,
    );
    assert!(
        finished.stderr.contains(XAI_JUDGE_MODEL),
        "the preflight refusal did not name the judge model: {}",
        finished.stderr
    );
    assert_eq!(
        stub.generations().len(),
        0,
        "an inexpressible judge must not make a provider generation"
    );
    let sessions = config
        .parent()
        .expect("config has a home directory")
        .join("sessions");
    assert!(
        !sessions.exists()
            || std::fs::read_dir(&sessions)
                .expect("the sessions directory is readable")
                .next()
                .is_none(),
        "judge wire preflight must not create a durable session: {}",
        sessions.display()
    );
}

#[test]
fn auto_without_a_judge_configuration_fails_before_any_judge_attempt() {
    let stub = Stub::start(Plan::default());
    let finished = auto_args(Ego::against(&stub)).run();

    assert_eq!(
        finished.code,
        Some(2),
        "missing judge configuration was accepted"
    );
    assert!(
        finished.stderr.contains("requires") && finished.stderr.contains("judge"),
        "missing judge failure did not name the required configuration: {}",
        finished.stderr
    );
    assert_eq!(
        stub.generations().len(),
        0,
        "a missing judge must not reach a provider"
    );
}

#[test]
fn auto_deny_records_judge_authority_and_never_executes_the_call() {
    let marker_home = tempfile::tempdir().expect("marker fixture directory");
    let marker = marker_home.path().join("judge-denied-executed");
    let stub = Stub::start(marker_plan(
        &marker,
        judge_response("deny", "the call is not admitted"),
        None,
    ));
    let ego = auto_run(&stub);
    let config = ego.home_file("config.toml", "[permissions]\njudge = \"ollama/qwen3\"\n");
    let finished = ego.run();

    assert_eq!(finished.code, Some(3), "stderr: {}", finished.stderr);
    let report = finished.report();
    assert_eq!(report["ok"], false);
    assert_eq!(report["exit_reason"], "permission");
    assert_eq!(report["denied_by"]["by"], "judge");
    assert!(
        !marker.exists(),
        "a denied call reached the process backend"
    );
    assert_eq!(
        stub.generations().len(),
        2,
        "deny is one judge attempt, never a retry"
    );
    assert_judge_decision(
        &durable_events(&config),
        Some(false),
        MODEL,
        &["main", "judge"],
    );
}

#[test]
fn auto_escalate_is_headless_permission_exit_without_execution_or_retry() {
    let marker_home = tempfile::tempdir().expect("marker fixture directory");
    let marker = marker_home.path().join("judge-executed");
    let stub = Stub::start(marker_plan(
        &marker,
        judge_response("escalate", "uncertain from the bounded facts"),
        None,
    ));
    let ego = auto_run(&stub);
    let config = ego.home_file("config.toml", "[permissions]\njudge = \"ollama/qwen3\"\n");
    let finished = ego.run();

    assert_eq!(finished.code, Some(3), "stderr: {}", finished.stderr);
    assert_eq!(
        stub.generations().len(),
        2,
        "escalation gets one judge attempt"
    );
    assert!(!marker.exists(), "escalated argv reached execution");
    assert_judge_decision(&durable_events(&config), None, MODEL, &["main", "judge"]);
}

#[test]
fn malformed_judge_responses_escalate_without_repair_or_a_second_attempt() {
    let malformed = [
        r#"{"verdict":"allow","reason":"ok","extra":true}"#,
        r#"{"verdict":"maybe","reason":"unknown verdict"}"#,
        r#"{"verdict":"allow"}"#,
        r#"{"verdict":"allow","reason":"ok","reason":"duplicate"}"#,
        r#"{"verdict":"allow","reason":"ok"} trailing"#,
        r#"{"verdict":"allow","reason":"ok"}{"verdict":"deny","reason":"second"}"#,
    ];

    for body in malformed {
        let stub = Stub::start(plan(Reply::Sse(text(body)), None));
        let finished = auto_run(&stub).run();

        assert_eq!(
            finished.code,
            Some(3),
            "malformed judge was accepted: {body}"
        );
        assert_eq!(
            stub.generations().len(),
            2,
            "malformed judge was repaired/retried: {body}"
        );
    }
}

#[test]
fn a_judge_tool_call_is_uncertainty_not_permission() {
    let stub = Stub::start(plan(
        Reply::Sse(tool_call("judge-tool", "read", r#"{"path":"/etc/hosts"}"#)),
        None,
    ));
    let finished = auto_run(&stub).run();

    assert_eq!(
        finished.code,
        Some(3),
        "judge tool call was treated as a verdict"
    );
    assert_eq!(
        stub.generations().len(),
        2,
        "judge tool call caused a retry"
    );
}

#[test]
fn judge_provider_failure_escalates_once_without_retrying_or_executing() {
    let stub = Stub::start(plan(Reply::Status(500), None));
    let ego = auto_run(&stub);
    let config = ego.home_file("config.toml", "[permissions]\njudge = \"ollama/qwen3\"\n");
    let finished = ego.run();

    assert_eq!(
        finished.code,
        Some(3),
        "judge provider failure was not fail-closed"
    );
    assert_eq!(
        stub.generations().len(),
        2,
        "provider failure caused a retry"
    );
    assert_judge_decision(&durable_events(&config), None, MODEL, &["main", "judge"]);
}

#[test]
fn judge_aggregate_output_cap_escalates_and_never_parses_a_prefix() {
    let oversized = format!(
        "{{\"verdict\":\"allow\",\"reason\":\"{}\"}}",
        "x".repeat(16 * 1024)
    );
    let stub = Stub::start(plan(Reply::Sse(text(&oversized)), None));
    let finished = auto_run(&stub).run();

    assert_eq!(
        finished.code,
        Some(3),
        "oversized judge response was accepted"
    );
    assert_eq!(
        stub.generations().len(),
        2,
        "oversized output caused a retry"
    );
}

#[test]
fn truncated_judge_stream_escalates_after_one_settled_attempt() {
    let truncated = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"{\\\"verdict\\\":\\\"allow\\\"\"},\"finish_reason\":null}]}\n\n";
    let stub = Stub::start(plan(Reply::Sse(truncated.to_string()), None));
    let finished = auto_run(&stub).run();

    assert_eq!(
        finished.code,
        Some(3),
        "truncated judge stream was accepted"
    );
    assert_eq!(
        stub.generations().len(),
        2,
        "truncated output caused a retry"
    );
}

#[test]
fn judge_reservation_uses_the_shared_budget_gate_and_budget_exits_before_judge_call() {
    let stub = Stub::start(plan(
        judge_response("allow", "this response must never be requested"),
        Some(Reply::Sse(text("done"))),
    ));
    let finished = hosted_auto_run(&stub).run();

    assert_eq!(
        finished.code,
        Some(5),
        "budget refusal was not reported: {}",
        finished.stderr
    );
    assert_eq!(finished.report()["exit_reason"], "budget");
    assert_eq!(
        stub.generations().len(),
        1,
        "the shared gate must refuse the judge generation before its provider call"
    );
}

#[test]
fn judge_reason_is_utf8_safe_and_bounded_in_the_durable_decision() {
    let reason = "€".repeat(500);
    let stub = Stub::start(plan(judge_response("deny", &reason), None));
    let ego = auto_run(&stub);
    let config = ego.home_file("config.toml", "[permissions]\njudge = \"ollama/qwen3\"\n");
    let finished = ego.run();

    assert_eq!(
        finished.code,
        Some(3),
        "deny with long reason did not route to permission"
    );
    let event = durable_events(&config)
        .into_iter()
        .find(|event| event["kind"]["kind"] == "PermissionDecided")
        .expect("judge decision is durable");
    let persisted = event["kind"]["reason"]
        .as_str()
        .expect("decision stores a reason");
    assert!(
        persisted.len() <= 1024,
        "reason exceeded the UTF-8 byte bound"
    );
    assert!(std::str::from_utf8(persisted.as_bytes()).is_ok());

    let events = durable_events(&config);
    assert_judge_decision(&events, Some(false), MODEL, &["main", "judge"]);
}
