use super::*;

// Catches: an unknown slash answer is consumed as a command and never reaches the tool.
#[test]
fn modal_unknown_slash_answer_reaches_question() {
    let call = serde_json::json!({
        "prompt": "Which absolute path?",
        "mode": "free_text",
        "options": [],
        "free_text": true,
    })
    .to_string();
    let (generations, _) = question_session(&call, |input, reader, output| {
        until_normalized(reader, output, 0, "Which absolute path?\n");
        // Registered aliases are commands, not free-text answers.
        let start = output.len();
        input.write_all(b"/history path\r").unwrap();
        input.flush().unwrap();
        until_normalized(reader, output, start, "unavailable while a question");
        input.write_all(b"/etc/hosts\r").unwrap();
        input.flush().unwrap();
    });
    let reply = generations[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == "question")
        .expect("the question reply reaches the model");
    assert_eq!(reply["content"], "/etc/hosts");
}

// Catches: Enter on a prefilled absolute cwd is discarded instead of preserving the amendment.
#[test]
fn modal_prefilled_absolute_path_is_preserved() {
    let mut session = modal_session(("exec", r#"{"argv":["pwd"],"cwd":"/","timeout_ms":5000}"#));
    session.press(b"use the tools\r");
    session.wait("[d] details");
    session.press(b"e");
    session.wait("argv: ");
    session.press(b"\r");
    session.wait("cwd: ");
    session.press(b"\r");
    session.wait("timeout_ms: ");
    session.press(b"\r");
    session.wait("background: ");
    session.press(b"false\r");
    session.wait("[d] details");
    assert_eq!(
        session.stub.generations().len(),
        1,
        "the amendment must ask again"
    );
    session.press(b"y");
    session.wait(TOOL_TURN_DONE);
    let (generations, _) = session.finish();
    let result = generations[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the amended call result");
    assert!(result.contains("/\n"), "cwd was not preserved: {result}");
}
