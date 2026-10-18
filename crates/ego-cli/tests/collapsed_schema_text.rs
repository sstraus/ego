#![cfg(unix)]

mod support;

use serde_json::Value;
use support::{Ego, Plan, Reply, Stub, text, tool_call};

const SERVER: &str = r#"
import hashlib
import json
import sys
from datetime import datetime

mode = sys.argv[1]
with open(sys.argv[3]) as fixture:
    search_response = json.load(fixture)
definition = {
    "name": "repo",
    "description": "List repositories",
    "inputSchema": {
        "type": "object",
        "properties": {"action": {"type": "string", "enum": ["list"]}},
        "required": ["action"],
        "additionalProperties": False,
    },
}
if mode.startswith("captured"):
    with open(sys.argv[2]) as fixture:
        captured = json.load(fixture)["result"]
    definition = json.loads(captured["content"][0]["text"])
if mode.startswith("schedule"):
    with open(sys.argv[2]) as fixture:
        definition = json.load(fixture)
management = [
    {
        "name": "search_tools",
        "description": "Find a tool",
        "inputSchema": {
            "type": "object",
            "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}},
            "required": ["query"],
            "additionalProperties": False,
        },
    },
    {
        "name": "get_tool_schema",
        "description": "Get a tool definition",
        "inputSchema": {
            "type": "object",
            "properties": {"tool_name": {"type": "string"}},
            "required": ["tool_name"],
            "additionalProperties": False,
        },
    },
    {
        "name": "call_tool",
        "description": "Call a selected tool",
        "inputSchema": {
            "type": "object",
            "properties": {
                "tool_name": {"type": "string"},
                "arguments": {"type": "object"},
            },
            "required": ["tool_name", "arguments"],
            "additionalProperties": False,
        },
    },
]

for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "server/discover":
        result = {
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {"listChanged": False}},
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {"io.modelcontextprotocol/serverInfo": {"name": "fixture", "version": "1"}},
        }
    elif method == "tools/list":
        result = {
            "resultType": "complete",
            "tools": management,
            "nextCursor": None,
            "ttlMs": 0,
            "cacheScope": "private",
        }
    elif method == "tools/call":
        operation = request["params"]["name"]
        if operation != "search_tools":
            assert request["params"]["arguments"]["tool_name"] == definition["name"]
        result = {"resultType": "complete", "isError": False}
        if operation == "search_tools":
            # Source-derived TUIC response: summaries and a sibling complete top_schema.
            response = dict(search_response)
            response["results"] = [{"name": definition["name"], "summary": definition["description"]}]
            response["catalogue_generation"] = hashlib.sha256(json.dumps(
                [definition], sort_keys=True, separators=(",", ":"), ensure_ascii=False
            ).encode()).hexdigest()
            if mode in ("schedule-baseline", "malformed", "multiple", "empty", "nontext"):
                response.pop("top_schema")
            else:
                response["top_schema"] = definition
            result["content"] = [{"type": "text", "text": json.dumps(response)}]
        elif operation == "call_tool":
            arguments = request["params"]["arguments"]["arguments"]
            if mode.startswith("schedule"):
                assert arguments["action"] == "create"
                scheduled = arguments["definition"]
                assert scheduled["cron"] == ""
                assert scheduled["enabled"] is True
                delta = (datetime.fromisoformat(scheduled["once_local"]) - datetime.now()).total_seconds()
                assert 3500 < delta <= 3600, delta
                result["content"] = [{"type": "text", "text": "fixture-scheduled-one-time-check"}]
            elif mode.startswith("captured"):
                assert arguments == {"type": "done", "text": "capture replay"}
                result["content"] = [{"type": "text", "text": "fixture-progress-recorded"}]
            else:
                assert arguments == {"action": "list"}
                result["content"] = [{"type": "text", "text": "fixture-repository"}]
        elif operation == "get_tool_schema" and mode == "captured":
            result.update(captured)
        elif operation == "get_tool_schema" and mode in ("text", "schedule-baseline"):
            result["content"] = [{"type": "text", "text": json.dumps(definition)}]
        elif operation == "get_tool_schema" and mode == "both":
            result["structuredContent"] = definition
            result["content"] = [{"type": "text", "text": "not JSON"}]
        elif operation == "get_tool_schema" and mode == "malformed":
            result["content"] = [{"type": "text", "text": "{broken"}]
        elif operation == "get_tool_schema" and mode == "multiple":
            result["content"] = [
                {"type": "text", "text": json.dumps(definition)},
                {"type": "text", "text": json.dumps(definition)},
            ]
        elif operation == "get_tool_schema" and mode == "empty":
            result["content"] = []
        elif operation == "get_tool_schema" and mode == "nontext":
            result["content"] = [{"type": "image", "data": "AA==", "mimeType": "image/png"}]
        else:
            raise AssertionError((operation, mode))
    else:
        raise AssertionError(method)
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#;

fn run_lookup(mode: &str) -> (support::Finished, Vec<Value>) {
    let mut completions = vec![Reply::Sse(tool_call(
        "schema-call",
        if mode.starts_with("search") {
            "search_tools"
        } else {
            "get_tool_schema"
        },
        if mode.starts_with("search") {
            r#"{"query":"list repositories"}"#
        } else if mode == "captured" {
            r#"{"tool_name":"progress"}"#
        } else {
            r#"{"tool_name":"repo"}"#
        },
    ))];
    if matches!(
        mode,
        "text" | "both" | "search" | "search-invalid" | "captured"
    ) {
        completions.push(Reply::Sse(tool_call(
            "repo-call",
            "call_tool",
            if mode == "captured" {
                r#"{"tool_name":"progress","arguments":{"type":"done","text":"capture replay"}}"#
            } else if mode == "search-invalid" {
                r#"{"tool_name":"repo","arguments":{"action":7}}"#
            } else {
                r#"{"tool_name":"repo","arguments":{"action":"list"}}"#
            },
        )));
        completions.push(Reply::Sse(text("REPO-LIST-COMPLETE")));
    } else {
        completions.push(Reply::Sse(tool_call(
            "changed-query",
            "search_tools",
            r#"{"query":"a different repository query"}"#,
        )));
        completions.push(Reply::Sse(tool_call(
            "schema-retry",
            "get_tool_schema",
            r#"{"tool_name":"repo"}"#,
        )));
        completions.push(Reply::Sse(text("LOOKUP-REFUSED-COMPLETE")));
    }
    let stub = Stub::start(Plan {
        completions,
        ..Plan::default()
    });
    let ego = Ego::against(&stub).args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
        "--output",
        "jsonl",
    ]);
    let server = ego.file("collapsed-server.py", SERVER);
    let captured = ego.file(
        "captured-schema.json",
        include_str!("fixtures/tuic-1.8.0-progress-schema-response.json"),
    );
    // TUIC 49f31508b1 search producer and needle regression; see fixture provenance.
    let search_response = ego.file(
        "search-response.json",
        include_str!("../../ego-mcp/tests/fixtures/tuic-search-top-schema.json"),
    );
    let selector = ego.file(
        "mcp.json",
        &serde_json::json!({
            "name": "fixture",
            "transport": "stdio",
            "command": "/usr/bin/python3",
            "args": [server, mode, captured, search_response],
            "inherit_env": []
        })
        .to_string(),
    );
    let finished = ego
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .arg("Get the repo tool schema")
        .run();
    (finished, stub.generations())
}

fn offers_tool(generation: &Value, name: &str) -> bool {
    generation["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == name))
}

#[test]
fn text_only_schema_lookup_publishes_the_inner_tool() {
    let (finished, generations) = run_lookup("text");
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        finished.stdout.contains("REPO-LIST-COMPLETE"),
        "{finished:?}"
    );
    assert!(
        generations
            .first()
            .is_some_and(|first| !offers_tool(first, "call_tool")),
        "call_tool appeared before lookup: {generations:?}"
    );
    assert!(
        generations
            .iter()
            .skip(1)
            .any(|next| offers_tool(next, "call_tool")),
        "call_tool was not published after text lookup: {generations:?}; {finished:?}"
    );
    assert!(
        generations.iter().any(
            |request| request["messages"]
                .as_array()
                .is_some_and(|messages| messages.iter().any(|message| {
                    message["role"] == "tool"
                        && message["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("fixture-repository"))
                }))
        ),
        "the inner repo call did not return its listing: {generations:?}; {finished:?}"
    );
}

#[test]
fn structured_schema_wins_over_invalid_text() {
    let (finished, generations) = run_lookup("both");
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        generations
            .iter()
            .skip(1)
            .any(|next| offers_tool(next, "call_tool")),
        "structured schema did not publish call_tool: {generations:?}; {finished:?}"
    );
}

#[test]
fn malformed_or_multiple_text_blocks_refuse_schema_publication() {
    for mode in ["malformed", "multiple", "empty", "nontext"] {
        let (finished, generations) = run_lookup(mode);
        assert!(
            generations
                .iter()
                .all(|next| !offers_tool(next, "call_tool")),
            "{mode} published call_tool: {generations:?}; {finished:?}"
        );
        let report = format!("{generations:?}");
        assert!(
            report.contains("collapsed schema lookup")
                && report.contains(match mode {
                    "malformed" => "not valid JSON",
                    "nontext" => "not JSON text",
                    _ => "exactly one JSON text content block",
                }),
            "{mode} did not report a named protocol error: {report}"
        );
    }
}

/// Catches: a complete inline search schema still needs a second discovery call.
#[test]
fn collapsed_search_inline_schema_removes_the_lookup_hop() {
    let (finished, generations) = run_lookup("search");
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(
        offers_tool(&generations[1], "call_tool"),
        "inline search did not admit dispatcher: {generations:?}; {finished:?}"
    );
    assert!(
        format!("{generations:?}").contains("fixture-repository"),
        "{finished:?}"
    );
    assert_eq!(
        generations.len(),
        3,
        "search, call, answer: {generations:?}"
    );
}

/// Catches: inline admission bypasses call_tool inner argument validation.
#[test]
fn collapsed_search_inline_schema_still_validates_dispatch_arguments() {
    let (finished, generations) = run_lookup("search-invalid");
    assert_eq!(finished.code, Some(0), "{finished:?}");
    let report = format!("{generations:?}");
    assert!(report.contains("collapsed inner arguments"), "{report}");
    assert!(
        !report.contains("fixture-repository"),
        "invalid call reached server: {report}"
    );
}

/// Catches: one-time scheduling still requires a schema lookup after inline discovery.
#[test]
fn collapsed_schedule_one_hour_needs_one_discovery_hop() {
    for (mode, inline) in [("schedule-baseline", false), ("schedule-inline", true)] {
        let mut replies = vec![Reply::Sse(tool_call(
            "search",
            "search_tools",
            r#"{"query":"schedule one-time check in one hour"}"#,
        ))];
        if !inline {
            replies.push(Reply::Sse(tool_call(
                "lookup",
                "get_tool_schema",
                r#"{"tool_name":"automations"}"#,
            )));
        }
        let once_local = (chrono::Local::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        let arguments = serde_json::json!({
            "tool_name":"automations", "arguments": {"action":"create", "definition": {
                "id":"fixture-check", "name":"One-hour check", "prompt":"Check once", "run_config":"fixture",
                "repository":"/fixture", "workspace":{"mode":"existing"}, "once_local":once_local,
                "cron":"", "enabled":true, "grace_secs":60, "overlap":"skip", "max_duration_secs":60
            }}
        });
        replies.push(Reply::Sse(tool_call(
            "schedule",
            "call_tool",
            &arguments.to_string(),
        )));
        replies.push(Reply::Sse(text("SCHEDULED")));
        let stub = Stub::start(Plan {
            completions: replies,
            ..Plan::default()
        });
        let ego = Ego::against(&stub).args([
            "--model",
            "ollama/qwen3",
            "--mode",
            "yolo",
            "--sandbox",
            "off",
            "--output",
            "jsonl",
        ]);
        let server = ego.file("schedule-server.py", SERVER);
        // Captured from the live TUIC get_tool_schema(automations) response on 2026-10-17.
        let schema = ego.file(
            "automations-tool.json",
            include_str!("fixtures/tuic-automations-tool.json"),
        );
        let search_response = ego.file(
            "search-response.json",
            include_str!("../../ego-mcp/tests/fixtures/tuic-search-top-schema.json"),
        );
        let selector = ego.file("mcp.json", &serde_json::json!({"name":"fixture","transport":"stdio","command":"/usr/bin/python3","args":[server,mode,schema,search_response],"inherit_env":[]}).to_string());
        let finished = ego
            .arg("--mcp")
            .arg(&format!("@{}", selector.display()))
            .arg("Schedule a one-time check in one hour")
            .run();
        assert_eq!(finished.code, Some(0), "{finished:?}");
        let generations = stub.generations();
        assert_eq!(generations.len(), if inline { 3 } else { 4 });
        assert!(
            format!("{generations:?}").contains("fixture-scheduled-one-time-check"),
            "{finished:?}"
        );
        for request in &generations {
            let messages = request["messages"].as_array().expect("messages");
            let timestamp = messages
                .iter()
                .filter_map(|message| message["content"].as_str())
                .flat_map(str::lines)
                .find_map(|line| line.strip_prefix("- Current local date-time: "))
                .expect("current time on each generation");
            let observed = chrono::DateTime::parse_from_rfc3339(timestamp)
                .expect("local date-time has an explicit UTC offset");
            assert!(
                (chrono::Local::now().signed_duration_since(observed))
                    .num_seconds()
                    .abs()
                    < 100
            );
        }
        assert!(offers_tool(
            &generations[if inline { 1 } else { 2 }],
            "call_tool"
        ));
    }
}

/// Catches standard Tool metadata or the captured JSON text envelope losing dispatch authority.
#[test]
fn captured_tuic_metadata_and_text_envelope_admit_valid_dispatch() {
    let (finished, generations) = run_lookup("captured");
    assert_eq!(finished.code, Some(0), "{finished:?}");
    assert!(!offers_tool(&generations[0], "call_tool"));
    assert!(
        generations
            .iter()
            .skip(1)
            .any(|next| offers_tool(next, "call_tool"))
    );
    assert!(
        generations.iter().any(
            |request| request["messages"]
                .as_array()
                .is_some_and(
                    |messages| messages.iter().any(|message| message["role"] == "tool"
                        && message["content"]
                            .as_str()
                            .is_some_and(|content| content.contains("fixture-progress-recorded")))
                )
        ),
        "{generations:?}; {finished:?}"
    );
}

/// Catches deterministic envelope incompatibility being mistaken for a retryable query failure.
#[test]
fn unsupported_collapsed_envelope_has_terminal_compatibility_diagnostic() {
    for mode in ["malformed", "multiple", "empty", "nontext"] {
        let (finished, generations) = run_lookup(mode);
        assert!(
            generations
                .iter()
                .all(|request| !offers_tool(request, "call_tool")),
            "{mode}: {finished:?}"
        );
        let report = format!("{generations:?}");
        assert!(
            report.contains("non-retryable compatibility error"),
            "{mode}: {report}"
        );
        assert!(
            report.contains("changing the search query cannot repair"),
            "{mode}: {report}"
        );
        let messages = generations.last().unwrap()["messages"].as_array().unwrap();
        let lookup_errors: Vec<_> = messages
            .iter()
            .filter(|message| {
                message["role"] == "tool"
                    && matches!(
                        message["tool_call_id"].as_str(),
                        Some("schema-call" | "schema-retry")
                    )
            })
            .map(|message| message["content"].as_str().unwrap())
            .collect();
        assert_eq!(lookup_errors.len(), 2, "{mode}: {lookup_errors:?}");
        assert_eq!(
            lookup_errors[0], lookup_errors[1],
            "query changed the compatibility failure"
        );
        assert!(
            lookup_errors
                .iter()
                .all(|error| error.contains("non-retryable compatibility error"))
        );
    }
}
