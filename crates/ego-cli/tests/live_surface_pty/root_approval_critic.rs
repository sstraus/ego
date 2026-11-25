//! Independent critic tests for story 1148-01fc (root approval continuation).
//!
//! Register in `live_surface_pty.rs` with
//! `#[path = "live_surface_pty/root_approval_critic.rs"] mod root_approval_critic;`.
//! Not compiled or run by the critic; the parent owns compilation.
use super::*;

/// One SSE transcript with several tool calls in one assistant message.
fn tool_batch(calls: &[(&str, &str, String)]) -> String {
    let calls = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            serde_json::json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            })
        })
        .collect::<Vec<_>>();
    let delta = serde_json::json!({
        "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": calls}, "finish_reason": null}]
    });
    format!(
        "data: {delta}\n\n\
         data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":4,\"total_tokens\":14}}}}\n\n\
         data: [DONE]\n\n"
    )
}

fn outside_target() -> (tempfile::TempDir, std::path::PathBuf) {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("approved.txt");
    std::fs::write(&target, "original").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    (outside, target)
}

fn write_call(target: &std::path::Path) -> String {
    serde_json::json!({"files":[{"path":target,"contents":"approved bytes"}]}).to_string()
}

fn tool_message(generation: &serde_json::Value, call_id: &str) -> Option<String> {
    generation["messages"]
        .as_array()?
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
        .map(|message| message["content"].to_string())
}

/// Catches: re-admission re-enters `execute` with the original CLI prompt
/// still in `args.prompt`, so the opening request is seeded a second time as a
/// new user message after the approved turn completes.
#[test]
fn readmission_does_not_resubmit_the_opening_prompt() {
    let (_outside, target) = outside_target();
    let places = support::Places::new();
    places.file("AGENTS.md", "# Root critic fixture\n");
    let image = places.file("opening.png", "");
    std::fs::write(&image, support::PNG_1X1).unwrap();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("needs-root", "write", &write_call(&target))),
            Reply::Sse(tool_call(
                "after-readmission",
                "write",
                &write_call(&target),
            )),
            Reply::Sse(text("CRITIC-OPENING-DONE")),
            Reply::Sse(text("CRITIC-FOLLOWUP-DONE")),
        ],
        tags_refusal: Some((
            200,
            serde_json::json!({
                "models": [{"name":"qwen3", "capabilities":["completion","tools","vision"]}]
            })
            .to_string(),
        )),
        ..Plan::default()
    });
    let opening = "CRITIC-OPENING-REQUEST write the approved file";
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "--mode",
        "yolo",
        "--trust-workspace",
        "--attach",
        image.to_str().unwrap(),
        opening,
    ]);
    ego.home_file(
        "config.toml",
        &format!("roots=[{{path='{}',access='read'}}]\n", target.display()),
    );
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until_normalized(&reader, &mut output, 0, "Permanent");
    let start = output.len();
    input.write_all(b"2").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "CRITIC-OPENING-DONE");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    assert_eq!(stub.generations().len(), 3, "opening prompt replayed");
    let followup = output.len();
    input.write_all(b"CRITIC-UNRELATED-FOLLOWUP\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, followup, "CRITIC-FOLLOWUP-DONE");
    until_prompt_from(&reader, &mut output, followup);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "approved bytes");
    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        4,
        "opening prompt replayed after re-admission: {}",
        String::from_utf8_lossy(&output)
    );
    let opening_messages = generations[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("CRITIC-OPENING-REQUEST")
        })
        .count();
    assert_eq!(opening_messages, 1, "{:?}", generations[2]["messages"]);
    let followup = generations[3]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("CRITIC-UNRELATED-FOLLOWUP")
        })
        .unwrap();
    assert!(
        !followup["content"].to_string().contains("image_url"),
        "opening attachment replayed: {followup}"
    );
    let opening = generations[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| {
            message["role"] == "user"
                && message["content"]
                    .to_string()
                    .contains("CRITIC-OPENING-REQUEST")
        })
        .unwrap();
    assert!(
        opening["content"].to_string().contains("image_url"),
        "opening attachment was not exercised: {opening}"
    );
}

/// Catches: a root boundary runs a later sibling under stale authority, drops
/// its result, or replays it when the model reissues the not-run batch.
#[test]
fn readmission_settles_unstarted_siblings_before_reissuing_effects() {
    let (_outside, target) = outside_target();
    let places = support::Places::new();
    places.file("AGENTS.md", "# Root critic sibling fixture\n");
    let marker = places.file("sibling-effect-count.txt", "");
    let mark = serde_json::json!({
        "argv": ["/bin/sh", "-c", "printf x >> sibling-effect-count.txt"],
        "timeout_ms": 5000
    })
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_batch(&[
                ("needs-root", "write", write_call(&target)),
                ("unstarted-sibling", "exec", mark.clone()),
            ])),
            Reply::Sse(tool_batch(&[
                ("fresh-write", "write", write_call(&target)),
                ("fresh-sibling", "exec", mark),
            ])),
            Reply::Sse(text("CRITIC-PARENT-DONE")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "--mode",
        "yolo",
        "--trust-workspace",
    ]);
    ego.home_file(
        "config.toml",
        &format!("roots=[{{path='{}',access='read'}}]\n", target.display()),
    );
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"run the batch\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "");
    input.write_all(b"2").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "CRITIC-PARENT-DONE");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());

    assert_eq!(std::fs::read_to_string(&target).unwrap(), "approved bytes");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    let deferred = tool_message(&generations[1], "unstarted-sibling").unwrap();
    assert!(deferred.contains("not run"), "{deferred}");
    let completed = tool_message(&generations[2], "fresh-sibling").unwrap();
    assert!(!completed.contains("not run"), "{completed}");
}
