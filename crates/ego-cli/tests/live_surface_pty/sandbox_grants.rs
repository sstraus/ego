#[cfg(target_os = "macos")]
use super::*;

// Catches: CLI wires command approval but drops the sandbox seat, so a real
// interactive denial says nobody is available instead of asking the TTY user.
#[cfg(target_os = "macos")]
#[test]
fn interactive_workspace_denial_asks_and_keeps_a_session_grant() {
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("regression_case-input");
    std::fs::write(&path, "REGRESSION-MARKER-SUCCESS").unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    let args = serde_json::json!({"argv": ["/bin/cat", path], "timeout_ms": 5000}).to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("first-read", "exec", &args)),
            Reply::Sse(tool_call("second-read", "exec", &args)),
            Reply::Sse(text("regression_case-DONE")),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# Sandbox grant fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "workspace",
        "--mode",
        "default",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust_start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust_start);
    let start = output.len();
    input.write_all(b"read the input twice\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "[y] yes");
    input.write_all(b"y").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "for this session");
    let prompt = normalized(&output[start..]);
    assert!(prompt.contains(path.to_str().unwrap()), "{prompt}");
    input.write_all(b"y").unwrap();
    input.flush().unwrap();
    // The second call still requires command approval, not another path grant.
    until_normalized(&reader, &mut output, start, "REGRESSION-MARKER-SUCCESS");
    let second = output.len();
    until_normalized(&reader, &mut output, second, "[y] yes");
    input.write_all(b"y").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, second, "regression_case-DONE");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let requests = stub.generations();
    assert_eq!(requests.len(), 3);
    for request in &requests[1..] {
        assert!(
            request.to_string().contains("REGRESSION-MARKER-SUCCESS"),
            "{request}"
        );
    }
    assert!(!normalized(&output[second..]).contains("for this session"));
}

// Catches: Ctrl+B leaves an outstanding sandbox question attached to detached
// work, so the composer never returns and a stale answer can widen the session.
#[cfg(target_os = "macos")]
#[test]
fn promoting_exec_with_pending_sandbox_question_retires_the_modal() {
    promoted_denial_has_no_terminal_authority(true);
}

// Catches: a promoted child reaches its denial later and opens a new path modal.
#[cfg(target_os = "macos")]
#[test]
fn promoting_exec_before_sandbox_denial_does_not_ask() {
    promoted_denial_has_no_terminal_authority(false);
}

#[cfg(target_os = "macos")]
fn promoted_denial_has_no_terminal_authority(pending: bool) {
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("regression_case-promotion-input");
    std::fs::write(&path, "PROMOTED-READ-MUST-NOT-RUN").unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    let args = if pending {
        serde_json::json!({"argv": ["/bin/cat", path], "timeout_ms": 10000})
    } else {
        serde_json::json!({"argv": ["/bin/sh", "-c",
            "printf ready > ready; while [ ! -f release ]; do sleep 0.05; done; /bin/cat \"$1\"",
            "promotion-fixture", path], "timeout_ms": 10000})
    }
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("promoted-read", "exec", &args)),
            Reply::Sse(text("PROMOTION-TURN-DONE")),
            Reply::Sse(text("NEXT-FOREGROUND-USABLE")),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# Sandbox promotion fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "workspace",
        "--mode",
        "default",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"read outside\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "[y] yes");
    input.write_all(b"y").unwrap();
    input.flush().unwrap();
    if pending {
        until_normalized(&reader, &mut output, start, "for this session");
    } else {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ego.workspace_path().join("ready").exists() {
            assert!(Instant::now() < deadline, "child never reached barrier");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let promotion = output.len();
    input.write_all(b"\x02").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, promotion, "PROMOTION-TURN-DONE");
    until_prompt_from(&reader, &mut output, promotion);
    let transcript = normalized(&output[promotion..]);
    let prefix = "background exec id: ";
    let offset = transcript.find(prefix).expect("promoted handle") + prefix.len();
    let id = &transcript[offset..offset + 36];
    if !pending {
        std::fs::write(ego.workspace_path().join("release"), "release").unwrap();
    }
    // Inspect until the task settles; the real denied child must not be retried.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let inspection = output.len();
        input
            .write_all(format!("/tasks {id}\r").as_bytes())
            .unwrap();
        input.flush().unwrap();
        until_prompt_from(&reader, &mut output, inspection);
        if normalized(&output[inspection..]).contains("exited code 1") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "detached denial did not settle: {}",
            normalized(&output)
        );
    }
    let next = output.len();
    input.write_all(b"continue normally\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, next, "NEXT-FOREGROUND-USABLE");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let after = normalized(&output[promotion..]);
    assert!(!after.contains("Allow read "), "{after}");
    assert!(!after.contains("PROMOTED-READ-MUST-NOT-RUN"), "{after}");
    assert_eq!(stub.generations().len(), 3);
}
