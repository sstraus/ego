use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

// Catches: staging an image retires the owned prompt, then a missing cursor
// reply closes the editor before the image draft or later input can run.
#[test]
fn image_staging_without_cursor_replies_keeps_the_prompt_live() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("BEFORE-IMAGE-COMPLETED")),
            Reply::Sse(text("IMAGE-RECEIVED")),
            Reply::Sse(text("AFTER-IMAGE-RECEIVED")),
        ],
        tags_refusal: Some((
            200,
            serde_json::json!({
                "models": [{ "name": "qwen3", "capabilities": ["completion", "tools", "vision"] }]
            })
            .to_string(),
        )),
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# missing cursor image fixture\n");
    let path = ego.file("cursor-image.png", "");
    std::fs::write(&path, support::PNG_1X1).unwrap();
    let replies = Arc::new(AtomicBool::new(true));
    let (session, mut input, reader) = PtySession::start_command_with_cursor_replies(
        ego.pty_command(),
        40,
        140,
        Some((40, 140)),
        Some(Arc::clone(&replies)),
    );
    let mut output = Vec::new();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"finish before image staging\r").unwrap();
    input.flush().unwrap();
    until_prompt_after_reply(&reader, &mut output, b"BEFORE-IMAGE-COMPLETED");
    until_mcp_composer_is_idle(&reader, &mut output);
    replies.store(false, Ordering::SeqCst);
    let staged = output.len();
    input
        .write_all(format!("\x1b[200~{}\x1b[201~", path.display()).as_bytes())
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, staged, "[image: cursor-image.png]");
    input.write_all(b"describe this image\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, staged, "IMAGE-RECEIVED");
    let later = output.len();
    input.write_all(b"input after image staging\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, later, "AFTER-IMAGE-RECEIVED");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success(), "{}", normalized(&output));
    let requests = stub.generations();
    assert_eq!(requests.len(), 3, "initial, image, then later prompt");
    assert!(requests[1].to_string().contains("data:image/png;base64,"));
    assert!(requests[1].to_string().contains("describe this image"));
    assert!(
        requests[2]
            .to_string()
            .contains("input after image staging")
    );
}

// Catches: a delayed/missing cursor reply on busy submission kills the input
// owner, leaving a queued prompt and a working process with a cooked tty.
#[test]
fn queued_prompt_during_provider_retry_survives_missing_cursor_replies() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::StatusRetryAfter {
                status: 503,
                seconds: 3,
            },
            Reply::Sse(text("RETRY-RECOVERED")),
            Reply::Sse(text("QUEUED-ANSWER")),
            Reply::Sse(text("LATER-ANSWER")),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# stuck turn regression\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
        "--retry",
        "1",
    ]);
    let replies = Arc::new(AtomicBool::new(true));
    let (session, mut input, reader) = PtySession::start_command_with_cursor_replies(
        ego.pty_command(),
        40,
        140,
        Some((40, 140)),
        Some(Arc::clone(&replies)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"first provider attempt\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "attempt 1/1");
    // The live host capture has CSI 6n without a timely reply. Do not give
    // the submission or the next read a cursor report; ordinary keys stay real.
    replies.store(false, Ordering::SeqCst);
    let queued = output.len();
    input.write_all(b"queued during retry\r").unwrap();
    // A capacity refusal waits a minute; Meta-r ends the wait.
    input.write_all(b"\x1br").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, queued, "QUEUED-ANSWER");
    let later = output.len();
    input.write_all(b"later input still works\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, later, "LATER-ANSWER");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success(), "{}", normalized(&output));
    let requests = stub.generations();
    assert_eq!(requests.len(), 4, "retry, queued prompt, then later input");
    assert!(requests[2].to_string().contains("queued during retry"));
    assert!(requests[3].to_string().contains("later input still works"));
}

// Catches: an idle background-completion wake reinitializes the prompt through
// a cursor query, so a missing reply kills the owner or prevents later input.
#[test]
fn background_wake_without_cursor_replies_keeps_later_input_live() {
    let notice = "BG DONE exit=0 log=/tmp/background-completion.log cmd=true";
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text("BEFORE-WAKE-COMPLETED")),
            Reply::Sse(text("WAKE-RECEIVED")),
            Reply::Sse(text("AFTER-WAKE-RECEIVED")),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# background wake regression\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    let replies = Arc::new(AtomicBool::new(true));
    let (session, mut input, reader) = PtySession::start_command_with_cursor_replies(
        ego.pty_command(),
        40,
        140,
        Some((40, 140)),
        Some(Arc::clone(&replies)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"finish a turn before the wake\r").unwrap();
    input.flush().unwrap();
    until_prompt_after_reply(&reader, &mut output, b"BEFORE-WAKE-COMPLETED");
    until_mcp_composer_is_idle(&reader, &mut output);
    replies.store(false, Ordering::SeqCst);
    let wake = output.len();
    // Recorded host completion delivery: clear the composer, text, then Enter.
    input.write_all(b"\x15").unwrap();
    input.write_all(notice.as_bytes()).unwrap();
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, wake, "WAKE-RECEIVED");
    until_prompt_from(&reader, &mut output, wake);
    let later = output.len();
    input
        .write_all(b"input after the background wake\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, later, "AFTER-WAKE-RECEIVED");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success(), "{}", normalized(&output));
    let requests = stub.generations();
    assert_eq!(
        requests.len(),
        3,
        "initial turn, wake, and later input each run once"
    );
    assert_eq!(count_exact_strings(&requests[1]["messages"], notice), 1);
    assert_eq!(
        count_exact_strings(&requests[2]["messages"], "input after the background wake"),
        1
    );
}
