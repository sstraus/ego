//! The loop guard's terminal result must survive the real headless process.

use crate::support;

use support::{Places, Plan, Reply, Stub, text, tool_call};

// Catches: distinct MCP refusals bypass the guard, or a sibling already
// buffered in the rejected response runs before the guard settles the turn.
#[cfg(unix)]
#[test]
fn progress_retry_guard_discards_entire_mcp_batch() {
    use serde_json::json;

    fn progress_args(index: usize) -> serde_json::Value {
        json!({"type":"done", "question":{"kind":"yes_no"}, "text":format!("Report {index}")})
    }
    let mut replies: Vec<_> = (0..7)
        .map(|index| {
            Reply::Sse(tool_call(
                &format!("progress-{index}"),
                "progress",
                &progress_args(index).to_string(),
            ))
        })
        .collect();
    let batch = json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[
        {"index":0,"id":"effect","type":"function","function":{"name":"mark","arguments":"{}"}},
        {"index":1,"id":"progress-7","type":"function","function":{"name":"progress","arguments":progress_args(7).to_string()}}
    ]},"finish_reason":null}]});
    replies.push(Reply::Sse(format!(
        "data: {batch}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )));
    replies.push(Reply::Sse(text("must not generate again")));
    let stub = Stub::start(Plan {
        completions: replies,
        ..Plan::default()
    });
    let ego = support::Ego::against(&stub);
    let log = ego.home_path().join("progress-calls");
    let marker = ego.home_path().join("sibling-effect");
    let server = ego.file("progress-server.sh", r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in *'"id"'*) ;; *) continue ;; esac
  id=${line#*\"id\":}
  id=${id%%,*}
  case "$line" in
    *server/discover*) result='{"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/list*) result='{"resultType":"complete","tools":[{"name":"progress","description":"Report progress.","inputSchema":{"type":"object","properties":{"type":{"type":"string"},"text":{"type":"string"},"question":{"type":"object","properties":{"kind":{"type":"string"}}}},"required":["type","text"],"additionalProperties":false}},{"name":"mark","description":"Write a marker.","inputSchema":{"type":"object","additionalProperties":false}}],"nextCursor":null,"ttlMs":0,"cacheScope":"private"}' ;;
    *tools/call*'"progress"'*)
      printf '%s\n' "$line" >> "$1"
      result='{"resultType":"complete","content":[{"type":"text","text":"question requires type blocked"}],"isError":true}' ;;
    *tools/call*'"mark"'*)
      printf 'executed\n' > "$2"
      result='{"resultType":"complete","content":[{"type":"text","text":"marked"}],"isError":false}' ;;
    *) continue ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#);
    let selector = ego.file(
        "progress-mcp.json",
        &json!({
            "name":"fixture", "transport":"stdio", "command":"/bin/sh",
            "args":[server, log, marker], "inherit_env":[]
        })
        .to_string(),
    );
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--mode",
            "yolo",
            "--sandbox",
            "off",
            "--output",
            "json",
            "--mcp",
            &format!("@{}", selector.display()),
            "finish the report",
        ])
        .run();
    assert_eq!(
        finished.code,
        Some(7),
        "{}\n{}",
        finished.stdout,
        finished.stderr
    );
    assert_eq!(finished.report()["exit_reason"], "guard");
    assert!(
        finished
            .stderr
            .contains("tool retry limit exceeded: progress"),
        "{}",
        finished.stderr
    );
    let calls: Vec<_> = std::fs::read_to_string(log)
        .expect("MCP server received progress calls")
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["params"]["arguments"].clone()
        })
        .collect();
    assert_eq!(calls, (0..7).map(progress_args).collect::<Vec<_>>());
    assert!(!marker.exists(), "the buffered MCP sibling was executed");
    let generations = stub.generations();
    assert_eq!(generations.len(), 8);
    assert!(
        generations[7]["messages"]
            .to_string()
            .contains("question requires type blocked")
    );
}

// Catches: a detected tool loop is incorrectly reported as process success.
#[test]
fn repeated_tool_loop_exits_nonzero_with_visible_cause() {
    let places = Places::new();
    let path = places.file("note.txt", "pending");
    let args = serde_json::json!({"path":path}).to_string();
    let mut replies: Vec<_> = (0..8)
        .map(|index| Reply::Sse(tool_call(&format!("loop-{index}"), "read", &args)))
        .collect();
    replies.push(Reply::Sse(text("done")));
    let stub = Stub::start(Plan {
        completions: replies,
        ..Plan::default()
    });
    let finished = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
            "--output",
            "json",
            "read the note",
        ])
        .run();
    assert_eq!(finished.code, Some(7), "{}", finished.stderr);
    assert_eq!(finished.report()["exit_reason"], "guard");
    assert!(
        finished.stderr.contains("repeated tool call limit"),
        "{}",
        finished.stderr
    );
    let journal = places
        .home_path()
        .join("sessions")
        .join(finished.report()["session"].as_str().expect("session id"))
        .join("events.jsonl");
    let records = std::fs::read_to_string(journal).expect("durable journal");
    assert!(
        records.lines().any(|line| {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            value["payload"]["event"]["kind"]["exit_reason"] == "Guard"
        }),
        "no durable Guard turn outcome"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 8);
    assert!(
        generations[3]["messages"]
            .to_string()
            .contains("Tool loop warning")
    );
}

// Catches: repeated undeclared tool names, answered as failed feedback, escape
// the existing loop guard and run forever instead of ending the turn.
#[cfg(unix)]
#[test]
fn repeated_undeclared_tool_calls_end_at_the_existing_guard_limit() {
    let places = Places::new();
    let mut replies: Vec<_> = (0..8)
        .map(|index| Reply::Sse(tool_call(&format!("bad-{index}"), "ls", "{}")))
        .collect();
    replies.push(Reply::Sse(text("done")));
    let stub = Stub::start(Plan {
        completions: replies,
        ..Plan::default()
    });
    let finished = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
            "--output",
            "json",
            "list the files",
        ])
        .run();
    assert_eq!(finished.code, Some(7), "{}", finished.stderr);
    assert_eq!(finished.report()["exit_reason"], "guard");
    assert!(
        finished.stderr.contains("repeated tool call limit"),
        "{}",
        finished.stderr
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 8);
    assert!(
        generations[3]["messages"]
            .to_string()
            .contains("Tool loop warning")
    );
}
