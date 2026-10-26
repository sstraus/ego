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
    // regression case/654: a child denied after the turn ended settles later and starts
    // one automatic continuation. A pending question retires the denial inside
    // the turn, so the foreground turn consumes that outcome and no wake follows.
    let mut completions = vec![
        Reply::Sse(tool_call("promoted-read", "exec", &args)),
        Reply::Sse(text("PROMOTION-TURN-DONE")),
    ];
    if !pending {
        completions.push(Reply::Sse(text("SETTLEMENT-CONTINUATION-DONE")));
    }
    completions.push(Reply::Sse(text("NEXT-FOREGROUND-USABLE")));
    let stub = Stub::start(Plan {
        completions,
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
        until_normalized(
            &reader,
            &mut output,
            promotion,
            "SETTLEMENT-CONTINUATION-DONE",
        );
        until_prompt_from(&reader, &mut output, promotion);
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
    let requests = stub.generations();
    assert_eq!(requests.len(), if pending { 3 } else { 4 });
    if !pending {
        let settled =
            |request: &serde_json::Value| request.to_string().matches("exited code 1").count();
        assert_eq!(settled(&requests[2]), 1, "{}", requests[2]);
        assert_eq!(
            settled(&requests[3]),
            1,
            "settlement delivered more than once"
        );
    }
}

// Catches: an interactive shell exec refusal offers Read and then Write instead
// of explaining the executable authority the user actually needs to change.
#[test]
fn interactive_shell_exec_refusal_shows_recovery_without_filesystem_grants() {
    interactive_exec_refusal(false);
}

// Catches: bad-interpreter exit 126 returns silently without exec recovery in
// the terminal, even though the command approval seat is interactive.
#[test]
fn interactive_interpreter_refusal_shows_recovery_without_filesystem_grants() {
    interactive_exec_refusal(true);
}

fn interactive_exec_refusal(bad_interpreter: bool) {
    let outside = tempfile::tempdir().unwrap();
    let tools = outside.path().canonicalize().unwrap();
    let program = tools.join("refused-program");
    std::fs::copy("/bin/echo", &program).unwrap();
    let places = support::Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        format!(
            "roots=[{{path='{}',access='read'}}]\nexec_paths=[]\n",
            tools.display()
        ),
    )
    .unwrap();
    // Replay Bash diagnostics recorded in usability round two, also retained
    // in ego-tools/src/exec/denial_tests.rs. Only the temporary program path is
    // substituted. This exercises real child stderr, not a mocked tool result;
    // it does not claim a real Seatbelt or Linux interpreter-chain denial.
    let diagnostic = if bad_interpreter {
        "/bin/bash: %s: /usr/bin/env: bad interpreter: Operation not permitted\\n"
    } else {
        "/bin/bash: %s: Operation not permitted\\n"
    };
    let shell = format!("printf '{diagnostic}' \"$1\" >&2; exit 126");
    let args = serde_json::json!({
        "argv": ["/bin/sh", "-c", shell, "recorded-exec-refusal", program],
        "timeout_ms": 5000,
    })
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("refused-exec", "exec", &args)),
            Reply::Sse(text(TOOL_TURN_DONE)),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--mode",
        "default",
        "--sandbox",
        "workspace",
    ]);
    let mut session = modal_session_on(ego, stub);
    session.press(b"run the recorded refusal\r");
    session.wait("[d] details");
    let approval = session.screen().join("\n");
    assert!(approval.contains("Allow run /bin/sh"), "{approval}");
    session.press(b"y");
    let after_approval = session.mark;
    // A broken implementation can stop at a path modal. Observe it and decline
    // it before asserting, so the RED replay also shuts its real child down.
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let offered_grant = loop {
        let shown = normalized(&session.output[after_approval..]);
        if shown.contains("for this session") {
            break true;
        }
        if shown.contains(TOOL_TURN_DONE) {
            break false;
        }
        let chunk = session
            .reader
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("exec refusal must reach recovery or a path modal");
        session.output.extend(chunk);
    };
    if offered_grant {
        session.press(b"n");
    }
    session.wait(TOOL_TURN_DONE);
    let transcript = normalized(&session.output[after_approval..]);
    let (generations, _) = session.finish();
    assert!(
        !offered_grant,
        "filesystem grant offered for executable: {transcript}"
    );
    let visible: String = transcript
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect();
    for expected in [
        "sandbox exec denied",
        "exec_paths",
        "read-only root",
        "ego config edit",
        "/quit",
        "ego resume",
        "--mode",
        "--sandbox",
        "--model",
        program.to_str().unwrap(),
        tools.to_str().unwrap(),
    ] {
        let expected_cells: String = expected.chars().filter(|ch| !ch.is_whitespace()).collect();
        assert!(
            visible.contains(&expected_cells),
            "missing {expected:?}: {transcript}"
        );
    }
    assert_eq!(generations.len(), 2, "one approval, no command retry");
    let result = generations[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .unwrap();
    assert!(result.contains("sandbox exec denied"), "{result}");
    assert!(result.contains("No command was retried"), "{result}");
}
