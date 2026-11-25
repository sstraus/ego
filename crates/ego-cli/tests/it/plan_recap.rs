//! The model's view of a plan that has just been finished.
use crate::support::{Ego, Plan, Reply, Stub, text, tool_call};

const RECAP_MARK: &str = "Every action of this worklist is completed";

/// Runs one headless turn whose first model reply is a `worklist` call, and returns the
/// text of every message the model saw in its second request.
fn second_request_text(arguments: &str, expected_requests: usize) -> String {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("plan-call", "worklist", arguments)),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .arg("Plan and finish")
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let requests = stub.generations();
    assert_eq!(requests.len(), expected_requests, "{requests:?}");
    requests[1]["messages"].to_string()
}

// Catches: the last step completes and the model sees only the plan JSON, so it has no
// cue to summarize and loses the thread; or the cue is sent while a step is still open.
#[test]
fn finishing_the_last_step_tells_the_model_to_recap_and_an_open_plan_does_not() {
    let finished = second_request_text(
        r#"{"operation":"replace","reason":null,"id":null,"status":null,"steps":[{"id":"a","text":"first","status":"completed","reason":null},{"id":"b","text":"second","status":"completed","reason":null}]}"#,
        2,
    );
    assert!(finished.contains(RECAP_MARK), "{finished}");
    let open = second_request_text(
        r#"{"operation":"replace","reason":null,"id":null,"status":null,"steps":[{"id":"a","text":"first","status":"completed","reason":null},{"id":"b","text":"second","status":"in_progress","reason":null}]}"#,
        3,
    );
    assert!(!open.contains(RECAP_MARK), "{open}");
    let blocked = second_request_text(
        r#"{"operation":"replace","reason":null,"id":null,"status":null,"steps":[{"id":"a","text":"first","status":"completed","reason":null},{"id":"b","text":"second","status":"blocked","reason":"external dependency"}]}"#,
        2,
    );
    assert!(!blocked.contains(RECAP_MARK), "{blocked}");
}

// Catches: the cue fires only for a whole-plan replace and never for the usual last
// `step` update, or it repeats every time a finished plan is touched again.
#[test]
fn the_cue_follows_the_update_that_finishes_the_plan_once() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "plan-1",
                "worklist",
                r#"{"operation":"replace","reason":null,"id":null,"status":null,"steps":[{"id":"a","text":"first","status":"completed","reason":null},{"id":"b","text":"second","status":"in_progress","reason":null}]}"#,
            )),
            Reply::Sse(tool_call(
                "plan-2",
                "worklist",
                r#"{"operation":"step","reason":null,"steps":null,"id":"b","status":"completed"}"#,
            )),
            Reply::Sse(tool_call(
                "plan-3",
                "worklist",
                r#"{"operation":"step","reason":null,"steps":null,"id":"b","status":"completed"}"#,
            )),
            Reply::Sse(text("done")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub);
    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .arg("Plan and finish")
        .run();
    assert_eq!(finished.code, Some(0), "{}", finished.stderr);
    let requests = stub.generations();
    assert_eq!(requests.len(), 4);
    let tool_results = |request: &serde_json::Value| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["content"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let after_replace = tool_results(&requests[1]);
    assert!(!after_replace[0].contains(RECAP_MARK), "{after_replace:?}");
    let after_step = tool_results(&requests[2]);
    assert!(after_step[1].contains(RECAP_MARK), "{after_step:?}");
    let after_repeat = tool_results(&requests[3]);
    assert!(!after_repeat[2].contains(RECAP_MARK), "{after_repeat:?}");
}
