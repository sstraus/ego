use super::*;

fn retain(name: &str, output: &[u8], journal: &str, details: &str) {
    if let Some(directory) = std::env::var_os("EGO_REGRESSION_CAPTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join(format!("{name}.ansi")), output).unwrap();
        std::fs::write(directory.join(format!("{name}.txt")), normalized(output)).unwrap();
        std::fs::write(directory.join(format!("{name}.jsonl")), journal).unwrap();
        std::fs::write(directory.join(format!("{name}-effects.txt")), details).unwrap();
    }
}

fn journal(places: &support::Places) -> String {
    std::fs::read_to_string(
        places
            .home_path()
            .join("sessions")
            .join(support::FIXTURE_SESSION_ID)
            .join("events.jsonl"),
    )
    .unwrap()
}

fn assert_no_grant(places: &support::Places) -> String {
    let journal = journal(places);
    assert!(!journal.contains("SessionRootGranted"), "{journal}");
    assert!(!journal.contains("PermanentRootGrantPending"), "{journal}");
    assert!(!journal.contains("PermanentRootGranted"), "{journal}");
    assert!(!places.home_path().join("config.toml").exists());
    journal
}

// Catches: an out-of-range terminal choice publishes a write root, or the
// rendered refusal omits the operation, access, reason, path, or choice labels.
#[test]
fn invalid_root_answer_keeps_write_denied_and_shows_complete_native_question() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("invalid-758.txt");
    std::fs::write(&target, "UNCHANGED-758").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let write = |id: &str| {
        Reply::Sse(tool_call(
            id,
            "write",
            &serde_json::json!({
                "path": target, "contents": "WRONG-758"
            })
            .to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Invalid root answer fixture\n");
    let stub = Stub::start(Plan {
        completions: vec![
            write("invalid-write"),
            Reply::Sse(text("INVALID-758-DENIED")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, Some((40, 160)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"attempt the external write\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "3. Permanent");
    drain_until_quiet(&reader, &mut output);
    let rendered = screen_rows(&output, 40, 160).join("\n");
    let question_words = rendered.split_whitespace().collect::<Vec<_>>().join(" ");
    for expected in [
        target.to_str().unwrap(),
        "write may not write",
        "read-write",
        "outside the workspace",
        "1. Deny",
        "2. Session",
        "3. Permanent",
        "including credentials",
    ] {
        assert!(
            question_words.contains(expected),
            "missing {expected}: {rendered}"
        );
    }
    // Nine is not an offered answer. It must not choose Session or Permanent.
    input.write_all(b"9").unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    assert_eq!(stub.generations().len(), 1, "invalid key settled the modal");
    assert_no_grant(&places);
    // Enter can only accept the unchanged default, Deny.
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "INVALID-758-DENIED");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "UNCHANGED-758");
    let saved = assert_no_grant(&places);
    let resumed = Stub::start(Plan {
        completions: vec![
            write("resumed-write"),
            Reply::Sse(text("RESUMED-WRITE-DENIED")),
        ],
        ..Plan::default()
    });
    let result = places
        .ego(&resumed)
        .subcommand("resume")
        .args([
            support::FIXTURE_SESSION_ID,
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "workspace",
            "--mode",
            "yolo",
        ])
        .arg("retry the write")
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "UNCHANGED-758");
    assert_no_grant(&places);
    retain(
        "invalid",
        &output,
        &saved,
        &format!(
            "RENDERED QUESTION:\n{rendered}\nInvalid 9: modal pending, one provider generation, no grant/config.\nAfter default Deny and fresh resume: external bytes UNCHANGED-758.\nTool results: {}",
            resumed.generations()[1]
        ),
    );
}

// Catches: tool stdout or model text arriving while a root modal owns input
// is interpreted as its choice, rather than remaining untrusted output.
#[test]
fn pending_root_modal_ignores_real_child_tool_output_and_model_choice_text() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("tool-origin-758.txt");
    std::fs::write(&target, "TOOL-ORIGIN-758-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let places = support::Places::new();
    places.file("AGENTS.md", "# Tool origin root fixture\n");
    let child_prompt = "Emit untrusted choice text";
    let gate = ReleaseGate::new();
    struct ReleaseOnDrop(Arc<ReleaseGate>);
    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let _release = ReleaseOnDrop(Arc::clone(&gate));
    let read = |id: &str| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"path": target}).to_string(),
        ))
    };
    let marker = "TOOL-758-OUTPUT session permanent 2 3";
    let stub = Stub::start_concurrent(Plan {
        completions: vec![
            Reply::Sse(tool_call("child", "task", &serde_json::json!({"name":"Emit choice text", "prompt":child_prompt, "background":true}).to_string())),
            read("pending-root"),
            Reply::Sse(text("REGRESSION-MARKER-DENIED")),
        ],
        prompt_completions: vec![(child_prompt.into(), vec![
            Reply::GatedPaced { head: String::new(), tail: tool_call("choice-output", "exec", &serde_json::json!({"argv":["/usr/bin/printf", format!("{marker}\n")]}).to_string()), gate: Arc::clone(&gate) },
            Reply::Sse(text("MODEL-758-OUTPUT session permanent 2 3")),
        ])],
        ..Plan::default()
    });
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, Some((40, 160)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input
        .write_all(b"start the child then read the external file\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "3. Permanent");
    let pending = output.len();
    gate.release();
    // The child actually executes printf after the parent modal is visible.
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let child_result = loop {
        let found = stub.generations().into_iter().find(|generation| {
            generation["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "tool"
                        && message["tool_call_id"] == "choice-output"
                        && message["content"]
                            .as_str()
                            .is_some_and(|content| content.contains(marker))
                })
            })
        });
        if let Some(found) = found {
            break found;
        }
        assert!(
            Instant::now() < deadline,
            "child stdout did not arrive: {}",
            normalized(&output)
        );
        if let Ok(bytes) = reader.recv_timeout(Duration::from_millis(100)) {
            output.extend(bytes);
        }
    };
    drain_until_quiet(&reader, &mut output);
    let during = screen_rows(&output, 40, 160).join("\n");
    assert!(
        during.contains("3. Permanent"),
        "tool text closed the modal: {during}"
    );
    assert!(!normalized(&output[pending..]).contains("REGRESSION-MARKER-DENIED"));
    assert_no_grant(&places);
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "REGRESSION-MARKER-DENIED");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let saved = assert_no_grant(&places);
    let resumed = Stub::start(Plan {
        completions: vec![
            read("still-denied"),
            Reply::Sse(text("REGRESSION-MARKER-DENIED")),
        ],
        ..Plan::default()
    });
    let result = places
        .ego(&resumed)
        .subcommand("resume")
        .args([
            support::FIXTURE_SESSION_ID,
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "workspace",
            "--mode",
            "yolo",
        ])
        .arg("retry the external read")
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(
        !resumed.generations()[1]
            .to_string()
            .contains("TOOL-ORIGIN-758-SECRET")
    );
    assert_no_grant(&places);
    retain(
        "tool-origin",
        &output,
        &saved,
        &format!(
            "MODAL AFTER CHILD STDOUT:\n{during}\nReal child tool result received while modal pending: {child_result}\nAfter cancellation: no grant/config. Fresh resume read denied: {}",
            resumed.generations()[1]
        ),
    );
}
