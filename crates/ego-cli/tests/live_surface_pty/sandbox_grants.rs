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
