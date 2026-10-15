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
#[cfg(not(windows))]
const ECHO_ARGV: &[&str] = &["/bin/echo", "judge exact argv"];
#[cfg(windows)]
const ECHO_ARGV: &[&str] = &[
    r"C:\Windows\System32\cmd.exe",
    "/C",
    "echo",
    "judge exact argv",
];

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
    let arguments = serde_json::json!({
        "argv": ECHO_ARGV,
        "cwd": null,
        "timeout_ms": 1000,
    });
    Reply::Sse(tool_call("judge-exec", "exec", &arguments.to_string()))
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

fn run_without_durable_analysis(ego: Ego) -> support::Finished {
    let cache = ego.home_path().join("permission-analysis/v1.json");
    let finished = ego.run();
    assert!(
        !cache.exists(),
        "a failed judge attempt created a durable opinion"
    );
    finished
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
        "1.40$",
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
        encoded.contains("Ask"),
        "judge omitted the user task context: {encoded}"
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

#[cfg(unix)]
#[test]
fn two_process_sessions_reuse_one_paid_analysis_and_mint_fresh_audit_authority() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let stub = Stub::start(Plan {
        completions: vec![
            exec_request(),
            judge_response("allow", "one exact observed image"),
            Reply::Sse(text("first done")),
            exec_request(),
            Reply::Sse(text("second done")),
        ],
        ..Plan::default()
    });
    let build = || {
        Ego::at(home.path(), workspace.path(), Some(&stub))
            .env("ANTHROPIC_API_KEY", "sk-test")
            .args([
                "--model",
                HOSTED_MODEL,
                "--thinking",
                "off",
                "--mode",
                "auto",
                "--sandbox",
                "off",
                "--output",
                "json",
                "Ask",
            ])
    };
    let first = build();
    first.home_file(
        "config.toml",
        "[permissions]\njudge = \"anthropic/claude-test-5\"\n",
    );
    first.home_file("models.toml", ANTHROPIC_FACTS);
    let finished = first.run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let cache_path = home.path().join("permission-analysis/v1.json");
    let before = std::fs::read(&cache_path).expect("complete image analysis was persisted");
    let finished = build().run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert_eq!(
        stub.generations().len(),
        5,
        "the second session must not generate a judge opinion"
    );
    assert_eq!(
        std::fs::read(cache_path).unwrap(),
        before,
        "a hit must not rewrite the cache"
    );
    let mut decisions = Vec::new();
    let mut judge_generations = 0;
    for session in std::fs::read_dir(home.path().join("sessions")).unwrap() {
        let session = session.unwrap();
        let path = session.path().join("events.jsonl");
        if !path.is_file() {
            continue;
        }
        for line in std::fs::read_to_string(path).unwrap().lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            let event = &record["payload"]["event"];
            if event["kind"]["kind"] == "GenerationStarted" && event["kind"]["role"] == "judge" {
                judge_generations += 1;
            }
            if event["kind"]["kind"] == "PermissionDecided" {
                assert_eq!(event["kind"]["allowed"], true);
                decisions.push((session.file_name(), event["kind"]["by"]["by"].clone()));
            }
        }
    }
    assert_eq!(judge_generations, 1);
    assert_eq!(decisions.len(), 2);
    assert_ne!(
        decisions[0].0, decisions[1].0,
        "the permits belong to distinct sessions"
    );
    assert!(decisions.iter().any(|(_, authority)| authority == "judge"));
    assert!(
        decisions
            .iter()
            .any(|(_, authority)| authority == "cached_judge")
    );
}

// Catches: session-owned read pins invalidating a persisted denial and paying twice.
#[cfg(unix)]
#[test]
fn rotating_session_read_pins_reuses_a_cached_denial_without_another_paid_judge() {
    let home = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let stub = Stub::start(Plan {
        completions: vec![
            exec_request(),
            judge_response("deny", "same observed image remains refused"),
            exec_request(),
        ],
        ..Plan::default()
    });
    let build = || {
        Ego::at(home.path(), workspace.path(), Some(&stub))
            .env("ANTHROPIC_API_KEY", "sk-test")
            .args([
                "--model",
                HOSTED_MODEL,
                "--thinking",
                "off",
                "--mode",
                "auto",
                "--sandbox",
                "off",
                "--output",
                "json",
                "Ask",
            ])
    };
    let first = build();
    first.home_file(
        "config.toml",
        "[permissions]\njudge = \"anthropic/claude-test-5\"\n",
    );
    first.home_file("models.toml", ANTHROPIC_FACTS);
    let finished = first.run();
    assert_eq!(finished.code, Some(3), "{}", finished.stderr);
    let cache_path = home.path().join("permission-analysis/v1.json");
    assert_eq!(finished.report()["denied_by"]["by"], "judge");
    let before = std::fs::read(&cache_path).expect("complete image analysis was persisted");
    let finished = build().run();
    assert_eq!(finished.code, Some(3), "{}", finished.stderr);
    assert_eq!(finished.report()["denied_by"]["by"], "cached_judge");
    assert_eq!(
        stub.generations().len(),
        3,
        "rotating only the session MCP read pin must not pay for another refusal"
    );
    assert_eq!(
        std::fs::read(cache_path).unwrap(),
        before,
        "a hit must not rewrite the cache"
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
fn auto_without_a_judge_configuration_starts_without_an_advisory_generation() {
    let stub = Stub::start(Plan::default());
    let finished = auto_args(Ego::against(&stub)).run();

    assert_eq!(
        finished.code,
        Some(0),
        "deterministic auto work without a judge did not start: {}",
        finished.stderr
    );
    assert!(
        stub.generations().len() == 1,
        "a missing judge must not produce an advisory generation: {:?}",
        stub.generations()
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
        let finished = run_without_durable_analysis(auto_run(&stub));

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
    let finished = run_without_durable_analysis(auto_run(&stub));

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
    let finished = run_without_durable_analysis(ego);

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
    for character in ["x", "€"] {
        let oversized = format!(
            "{{\"verdict\":\"allow\",\"reason\":\"{}\"}}",
            character.repeat(16 * 1024)
        );
        let stub = Stub::start(plan(Reply::Sse(text(&oversized)), None));
        let ego = auto_run(&stub);
        let config = ego.home_path().join("config.toml");
        let finished = run_without_durable_analysis(ego);
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
        let events = durable_events(&config);
        let response = events
            .iter()
            .find(|event| event["kind"]["kind"] == "JudgeResponse")
            .unwrap();
        let raw = response["kind"]["text"].as_str().unwrap();
        assert!(!raw.is_empty());
        assert!(raw.len() <= 16 * 1024);
        assert!(
            oversized.starts_with(raw),
            "raw evidence is not the original prefix"
        );
        assert_eq!(response["kind"]["truncated"], true);
    }
}

#[test]
fn truncated_judge_stream_escalates_after_one_settled_attempt() {
    let truncated = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"{\\\"verdict\\\":\\\"allow\\\"\"},\"finish_reason\":null}]}\n\n";
    let stub = Stub::start(plan(Reply::Sse(truncated.to_string()), None));
    let finished = run_without_durable_analysis(auto_run(&stub));

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
    // The first response reports a real settled cost of USD 0.25 input
    // plus USD 0.00004 output. The main hold fits USD 1.40, but that
    // committed spend plus the judge's USD 1.28 output hold cannot fit.
    let mut replies = plan(
        judge_response("allow", "this response must never be requested"),
        Some(Reply::Sse(text("done"))),
    );
    if let Reply::Sse(response) = &mut replies.completions[0] {
        *response = response.replace("\"prompt_tokens\":10", "\"prompt_tokens\":200000");
    }
    let stub = Stub::start(replies);
    let finished = run_without_durable_analysis(hosted_auto_run(&stub));

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

#[test]
fn unset_judge_routes_unknown_exec_to_main_model_instead_of_human() {
    let stub = Stub::start(plan(judge_response("deny", "not authorized"), None));
    let ego = auto_args(Ego::against(&stub));
    let config = ego.home_file("config.toml", "");
    let finished = ego.run();
    assert_eq!(stub.generations().len(), 2, "stderr: {}", finished.stderr);
    assert_judge_decision(
        &durable_events(&config),
        Some(false),
        MODEL,
        &["main", "judge"],
    );
}

#[test]
fn explicit_none_judge_does_not_generate_an_advisory_call() {
    let stub = Stub::start(Plan {
        completions: vec![exec_request()],
        ..Plan::default()
    });
    let ego = auto_args(Ego::against(&stub));
    ego.home_file("config.toml", "[permissions]\njudge = \"none\"\n");
    let finished = ego.run();
    assert_eq!(finished.code, Some(3), "stderr: {}", finished.stderr);
    assert_eq!(stub.generations().len(), 1);
}

// Catches: a single provider JSON fence escalates instead of authorizing its exact call.
#[test]
fn fenced_judge_response_executes_and_persists_raw_text() {
    // Recorded from OpenRouter anthropic/claude-haiku-4.5, 2026-10-10,
    // with production JUDGE_INSTRUCTIONS and a bounded pwd action.
    let raw = include_str!("fixtures/judge-haiku-response.txt");
    let stub = Stub::start(plan(Reply::Sse(text(raw)), Some(Reply::Sse(text("done")))));
    let ego = auto_run(&stub);
    let config = ego.home_path().join("config.toml");
    let finished = ego.run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    assert_judge_decision(
        &durable_events(&config),
        Some(true),
        MODEL,
        &["main", "judge", "main"],
    );
    assert_raw_judge_response(&durable_events(&config), raw);
    assert!(
        !finished.stdout.contains("```json"),
        "raw judge output became public"
    );
    assert_eq!(stub.generations().len(), 3);
}

// Catches: a malformed paid judge response is discarded rather than retained for diagnosis.
#[test]
fn malformed_judge_response_persists_raw_text_without_public_disclosure() {
    let raw = "unparseable private judge response";
    let stub = Stub::start(plan(Reply::Sse(text(raw)), None));
    let ego = auto_run(&stub);
    let config = ego.home_path().join("config.toml");
    let finished = ego.run();
    assert_eq!(finished.code, Some(3));
    assert_raw_judge_response(&durable_events(&config), raw);
    assert!(!finished.stdout.contains(raw));
    assert_eq!(stub.generations().len(), 2);
}

fn assert_raw_judge_response(events: &[serde_json::Value], raw: &str) {
    let response = events
        .iter()
        .find(|event| event["kind"]["kind"] == "JudgeResponse")
        .expect("raw judge response is durable even when parsing fails");
    assert_eq!(response["kind"]["text"], raw);
    assert_eq!(response["kind"]["truncated"], false);
    let started = events
        .iter()
        .find(|event| {
            event["kind"]["kind"] == "GenerationStarted" && event["kind"]["role"] == "judge"
        })
        .unwrap();
    assert_eq!(response["kind"]["attempt"], started["kind"]["attempt"]);
}
