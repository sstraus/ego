#![cfg(unix)]

use crate::support;

use support::{Ego, Plan, Reply, Stub, text, tool_call};

#[test]
fn permitted_current_http_mcp_tool_reaches_server_in_pty() {
    let stub = Stub::start_mcp_tool(Plan {
        completions: vec![
            Reply::Sse(tool_call("search-call", "search_tools", "{}")),
            Reply::Sse(text("HTTP-SEARCH-COMPLETE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
    ]);
    ego.file("AGENTS.md", "# HTTP MCP PTY fixture\n");
    let config = serde_json::json!({
        "name": "fixture",
        "transport": "http",
        "url": format!("http://{}/mcp", stub.address),
        "auth": "none",
        "allow_insecure_loopback": true
    });
    let selector = ego.file("mcp.json", &config.to_string());

    let finished = ego
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .run_pty("search tools");

    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    assert!(
        finished.stdout.contains("HTTP-SEARCH-COMPLETE"),
        "{}",
        finished.stdout
    );
    let requests = stub.requests();
    let calls = requests
        .iter()
        .filter(|request| request.path == "/mcp")
        .filter(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body["method"] == "tools/call")
        })
        .count();
    assert_eq!(
        calls, 1,
        "the selected HTTP request was not sent once; terminal: {}; requests: {requests:?}",
        finished.stdout
    );
    assert!(
        stub.generations()
            .iter()
            .any(|generation| generation.to_string().contains("lookup-complete")),
        "the provider did not receive the HTTP server's result"
    );
}

// Catches: with `mcp_tasks` on, a server's task is never polled or its result never reaches
// the model, or the model's own tool call blocks until the task ends.
#[test]
fn an_http_mcp_task_returns_at_once_and_its_result_reaches_the_model_at_a_later_generation() {
    let stub = Stub::start_mcp_task(Plan {
        completions: vec![
            Reply::Sse(tool_call("task-call", "search_tools", "{}")),
            // Long enough for the task's 500 ms poll interval to pass before the next request.
            support::paced_tool_call(
                "second-call",
                "search_tools",
                "{}",
                std::time::Duration::from_millis(1500),
            ),
            Reply::Sse(text("TASK-FLOW-COMPLETE")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--mode",
        "yolo",
        "--sandbox",
        "off",
    ]);
    ego.home_file("config.toml", "mcp_tasks = true\n");
    ego.file("AGENTS.md", "# HTTP MCP task fixture\n");
    let config = serde_json::json!({
        "name": "fixture",
        "transport": "http",
        "url": format!("http://{}/mcp", stub.address),
        "auth": "none",
        "allow_insecure_loopback": true
    });
    let selector = ego.file("mcp.json", &config.to_string());

    let finished = ego
        .arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .run_pty("run the slow search");

    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    let generations: Vec<String> = stub.generations().iter().map(ToString::to_string).collect();
    assert_eq!(generations.len(), 3, "{generations:?}");
    // The first tool answer is the immediate task receipt, not the result.
    assert!(
        generations[1].contains("MCP task task-"),
        "{}",
        generations[1]
    );
    assert!(
        !generations[1].contains("task-result-text"),
        "{}",
        generations[1]
    );
    // A later generation carries the settled task once.
    assert_eq!(
        generations[2].matches("task-result-text").count(),
        1,
        "{}",
        generations[2]
    );
    let polls = stub
        .requests()
        .iter()
        .filter(|request| request.path == "/mcp")
        .filter(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body["method"] == "tasks/get")
        })
        .count();
    assert!(polls >= 1, "the task was never polled");
    // The second task was still live when the session ended: ego cancelled it.
    let cancels = stub
        .requests()
        .iter()
        .filter(|request| request.path == "/mcp")
        .filter(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body["method"] == "tasks/cancel")
        })
        .count();
    assert_eq!(cancels, 1, "a live task must be cancelled once at exit");
}

/// One idle interactive session whose model starts an MCP task and then
/// answers "STARTED", left at its prompt for `idle` before EOF.
fn idle_task_session(stub: &Stub, idle: std::time::Duration) -> support::Finished {
    let ego = Ego::against(stub)
        .conversing()
        .idle_before_exit(idle)
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--mode",
            "yolo",
            "--sandbox",
            "off",
        ]);
    ego.home_file("config.toml", "mcp_tasks = true\n");
    ego.file("AGENTS.md", "# HTTP MCP task fixture\n");
    let config = serde_json::json!({
        "name": "fixture",
        "transport": "http",
        "url": format!("http://{}/mcp", stub.address),
        "auth": "none",
        "allow_insecure_loopback": true
    });
    let selector = ego.file("mcp.json", &config.to_string());
    ego.arg("--mcp")
        .arg(&format!("@{}", selector.display()))
        .run_pty("run the slow search")
}

fn task_polls(stub: &Stub) -> usize {
    stub.requests()
        .iter()
        .filter(|request| request.path == "/mcp")
        .filter(|request| {
            serde_json::from_str::<serde_json::Value>(&request.body)
                .is_ok_and(|body| body["method"] == "tasks/get")
        })
        .count()
}

// Catches: a task that settles while the session waits at its prompt is
// delivered only at the next prompt the user types, or more than once.
#[test]
fn a_task_that_settles_while_the_session_is_idle_starts_one_generation_with_its_result() {
    let stub = Stub::start_mcp_task(Plan {
        completions: vec![
            Reply::Sse(tool_call("task-call", "search_tools", "{}")),
            Reply::Sse(text("STARTED")),
            Reply::Sse(text("DELIVERED")),
        ],
        ..Plan::default()
    });

    let finished = idle_task_session(&stub, std::time::Duration::from_millis(3000));

    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    let generations: Vec<String> = stub.generations().iter().map(ToString::to_string).collect();
    assert_eq!(generations.len(), 3, "{generations:?}");
    assert!(
        !generations[1].contains("task-result-text"),
        "{}",
        generations[1]
    );
    assert_eq!(
        generations[2].matches("task-result-text").count(),
        1,
        "{}",
        generations[2]
    );
}

// Catches: a wake per poll, so a task that is still working costs a model
// generation every poll interval while the session is idle; and an idle
// session that never polls its live task.
#[test]
fn a_task_still_working_while_the_session_is_idle_is_polled_and_costs_no_generation() {
    let stub = Stub::start_mcp_task_working(Plan {
        completions: vec![
            Reply::Sse(tool_call("task-call", "search_tools", "{}")),
            Reply::Sse(text("STARTED")),
        ],
        ..Plan::default()
    });

    let finished = idle_task_session(&stub, std::time::Duration::from_millis(2500));

    assert_eq!(finished.code, Some(0), "{}", finished.stdout);
    assert_eq!(stub.generations().len(), 2, "a poll started a generation");
    assert!(
        task_polls(&stub) >= 2,
        "the idle session did not poll its live task: {} polls",
        task_polls(&stub)
    );
}
