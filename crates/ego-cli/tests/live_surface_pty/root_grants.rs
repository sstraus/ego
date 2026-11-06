use super::*;

/// Wait until a granted root rebuilds the same session in this process. A
/// change of Linux backend (bwrap to Landlock for a first read root) asks the
/// owner once; any other wider root re-admits with a note.
pub(super) fn readmit_after_root_grant(
    reader: &mpsc::Receiver<Vec<u8>>,
    output: &mut Vec<u8>,
    input: &mut PtyWriter,
    start: usize,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let words = normalized(&output[start..])
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if words.contains("Allow Re-admit execution policy") {
            input.write_all(b"y\r").unwrap();
            input.flush().unwrap();
            let approved = output.len();
            until_prompt_from(reader, output, approved);
            return;
        }
        if words.contains("Re-admitting under equal or narrower authority.") {
            // The prompt can share a read with the note; start after the note.
            let note_end = output
                .windows(b"authority.".len())
                .rposition(|window| window == b"authority.")
                .expect("re-admission note")
                + b"authority.".len();
            until_prompt_from(reader, output, note_end);
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        match reader.recv_timeout(remaining) {
            Ok(chunk) => output.extend(chunk),
            Err(error) => panic!(
                "no re-admission after the root grant: {error}; output: {:?}",
                normalized(&output[start..])
            ),
        }
    }
}

/// Catches: recording a root choice only in profile config, widening the live
/// PolicyEngine, or leaking the session root into a new conversation.
#[test]
fn terminal_session_root_is_denied_live_and_admitted_only_on_same_id_resume() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("policy-mark.txt");
    std::fs::write(&target, "POLICY-MARK-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let read = |id: &str| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"files":[{"path": target.to_str().unwrap()}]}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Root consent fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            read("denied-before"),
            Reply::Sse(text("LIVE-DENIED-MARK")),
            Reply::Sse(text("RECORDED-MARK")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&first).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"read the external file\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    let offered = normalized(&output[start..]);
    assert!(offered.contains(target.to_str().unwrap()), "{offered}");
    for choice in ["Deny", "Session", "Permanent"] {
        assert!(offered.contains(choice), "missing {choice}: {offered}");
    }
    input.write_all(b"2\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "LIVE-DENIED-MARK");
    readmit_after_root_grant(&reader, &mut output, &mut input, start);
    // A turn after re-admission records the re-admitted execution
    // observation, so the next resume compares against it.
    let recorded = output.len();
    input.write_all(b"record the readmission\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, recorded, "RECORDED-MARK");
    until_prompt_from(&reader, &mut output, recorded);
    let saved_receipt = first.generations()[1].to_string();
    assert!(saved_receipt.contains("Session root"), "{saved_receipt}");
    assert!(
        saved_receipt.contains("re-admits session"),
        "{saved_receipt}"
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(
        !first.generations()[1]
            .to_string()
            .contains("POLICY-MARK-SECRET"),
        "the granting turn saw the external bytes"
    );
    assert!(
        !places.home_path().join("config.toml").exists(),
        "session grant leaked into the profile"
    );

    let resumed = Stub::start(Plan {
        completions: vec![read("admitted-on-resume"), Reply::Sse(text("RESUMED-MARK"))],
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
        .arg("read the external file now")
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(
        resumed.generations()[1]
            .to_string()
            .contains("POLICY-MARK-SECRET"),
        "same-id resume did not admit the external read"
    );
    let writes = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "read-only-grant",
                "write",
                &serde_json::json!({"files": [{
                    "path": target.to_str().unwrap(), "contents": "WRONG-MARK"
                }]})
                .to_string(),
            )),
            Reply::Sse(text("WRITE-REFUSED-MARK")),
        ],
        ..Plan::default()
    });
    let denied_write = places
        .ego(&writes)
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
        .arg("overwrite the granted read-only file")
        .run();
    assert_eq!(denied_write.code, Some(0), "{}", denied_write.stderr);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "POLICY-MARK-SECRET"
    );
    let other = Stub::start(Plan {
        completions: vec![
            read("different-session"),
            Reply::Sse(text("OTHER-DENIED-MARK")),
        ],
        ..Plan::default()
    });
    let result = places
        .ego(&other)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "workspace",
            "--mode",
            "yolo",
            "read the external file",
        ])
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(
        !other.generations()[1]
            .to_string()
            .contains("POLICY-MARK-SECRET"),
        "a different session inherited the grant"
    );
}

/// Catches: after a fresh resume activates a session root, /new and /fork
/// reuse the same process-wide policy and OS guard for a different session.
#[test]
fn resumed_session_root_refuses_in_process_session_switches() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("switch-mark.txt");
    std::fs::write(&target, "SWITCH-MARK-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let places = support::Places::new();
    places.file("AGENTS.md", "# Session switch fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "ask-root",
                "read",
                &serde_json::json!({"files":[{"path": target}]}).to_string(),
            )),
            Reply::Sse(text("SWITCH-MARK-PENDING")),
            Reply::Sse(text("RECORDED-MARK")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&first).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"read external file\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    input.write_all(b"2\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "SWITCH-MARK-PENDING");
    readmit_after_root_grant(&reader, &mut output, &mut input, start);
    // A turn after re-admission records the re-admitted execution
    // observation, so the next resume compares against it.
    let recorded = output.len();
    input.write_all(b"record the readmission\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, recorded, "RECORDED-MARK");
    until_prompt_from(&reader, &mut output, recorded);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());

    let resumed = Stub::start(Plan::default());
    let ego = places.ego(&resumed).subcommand("resume").args([
        support::FIXTURE_SESSION_ID,
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"/new\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "exit and resume in a new process",
    );
    let next = output.len();
    input.write_all(b"/fork\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        next,
        "exit and resume in a new process",
    );
    let compact = output.len();
    input.write_all(b"/compact\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        compact,
        "session roots cannot cross a compaction session",
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}

/// Catches: a durable background child inherits a session-only root from
/// its parent instead of resolving its own distinct session authority.
#[cfg(target_os = "macos")]
#[test]
fn background_task_cannot_read_the_parent_session_root() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("fork-child-mark.txt");
    std::fs::write(&target, "FORK-CHILD-MARK-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let places = support::Places::new();
    places.file("AGENTS.md", "# Child root fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "ask-root",
                "read",
                &serde_json::json!({"files":[{"path": target}]}).to_string(),
            )),
            Reply::Sse(text("CHILD-ROOT-PENDING-MARK")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&first).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"read external fixture\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    input.write_all(b"2\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "CHILD-ROOT-PENDING-MARK");
    readmit_after_root_grant(&reader, &mut output, &mut input, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());

    let child_prompt = "Try read and cat on the external fixture";
    let after = Stub::start_concurrent(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "fork-task",
                "task",
                &serde_json::json!({
                    "name": "Check root", "prompt": child_prompt, "background": true
                })
                .to_string(),
            )),
            Reply::Sse(text("PARENT-TASK-DONE-MARK")),
        ],
        prompt_completions: vec![(
            child_prompt.into(),
            vec![
                Reply::Sse(tool_call(
                    "child-read",
                    "read",
                    &serde_json::json!({"files":[{"path": target}]}).to_string(),
                )),
                Reply::Sse(tool_call(
                    "child-cat",
                    "exec",
                    &serde_json::json!({"argv": ["/bin/cat", target], "timeout_ms": 5000})
                        .to_string(),
                )),
                Reply::Sse(text("CHILD-ROOT-REFUSED-MARK")),
            ],
        )],
        ..Plan::default()
    });
    let resumed = places
        .ego(&after)
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
        .arg("ask a task to check the external fixture")
        .run();
    assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
    let child_generations = after
        .generations()
        .into_iter()
        .filter(|generation| {
            generation["messages"].as_array().is_some_and(|messages| {
                messages
                    .iter()
                    .any(|message| message["role"] == "user" && message["content"] == child_prompt)
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(child_generations.len(), 3, "{}", resumed.stderr);
    for (index, call_id) in [(1, "child-read"), (2, "child-cat")] {
        let result = child_generations[index]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == call_id)
            .and_then(|message| message["content"].as_str())
            .expect("durable child tool result");
        assert!(!result.is_empty(), "empty {call_id} result");
        assert!(
            !result.contains("FORK-CHILD-MARK-SECRET"),
            "durable child {call_id} read its parent's root: {result}; stderr: {}",
            resumed.stderr
        );
    }
}

/// Catches: cancelling a root question or model output containing a choice
/// is treated as a terminal approval and later widens the resumed session.
#[test]
fn cancelled_root_question_and_model_text_cannot_grant_authority() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("cancelled-mark.txt");
    std::fs::write(&target, "CANCELLED-MARK-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let read = |id: &str| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"files":[{"path": target}]}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Cancelled root fixture\n");
    let stub = Stub::start(Plan {
        completions: vec![
            read("cancelled-root"),
            Reply::Sse(text("session permanent approve CANCEL-MARK")),
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
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"read external file\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "CANCEL-MARK");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(!places.home_path().join("config.toml").exists());
    let resumed = Stub::start(Plan {
        completions: vec![read("still-denied"), Reply::Sse(text("DENIED-MARK"))],
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
        .arg("read the external file")
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(
        !resumed.generations()[1]
            .to_string()
            .contains("CANCELLED-MARK-SECRET")
    );
}

/// Catches: a permanent answer changes the live run, is not saved by the
/// atomic user-config transaction, or admits an unselected sibling path.
#[test]
fn permanent_root_is_readmitted_in_process_and_binds_the_exact_path() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("chosen-mark.txt");
    let sibling = outside.path().join("other-mark.txt");
    std::fs::write(&target, "CHOSEN-MARK-SECRET").unwrap();
    std::fs::write(&sibling, "OTHER-MARK-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let sibling = std::fs::canonicalize(sibling).unwrap();
    let read = |id: &str, path: &std::path::Path| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"files":[{"path": path}]}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Permanent root fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            read("before", &target),
            Reply::Sse(text("PERMANENT-PENDING-MARK")),
            read("readmitted", &target),
            Reply::Sse(text("PERMANENT-READMITTED-MARK")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&first).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"read chosen file\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    input.write_all(b"3\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "PERMANENT-PENDING-MARK");
    readmit_after_root_grant(&reader, &mut output, &mut input, start);
    let saved_receipt = first.generations()[1].to_string();
    assert!(saved_receipt.contains("Permanent root"), "{saved_receipt}");
    assert!(
        saved_receipt.contains("re-admits session"),
        "{saved_receipt}"
    );
    assert!(
        !saved_receipt.contains("CHOSEN-MARK-SECRET"),
        "{saved_receipt}"
    );
    let again = output.len();
    input
        .write_all(b"read chosen file after readmission\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, again, "PERMANENT-READMITTED-MARK");
    until_prompt_from(&reader, &mut output, again);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(places.home_path().join("config.toml").exists());
    assert!(
        first.generations()[3]
            .to_string()
            .contains("CHOSEN-MARK-SECRET"),
        "the same process did not re-admit the permanent root"
    );

    let resumed = Stub::start(Plan {
        completions: vec![
            read("admitted", &target),
            Reply::Sse(text("CHOSEN-READ-MARK")),
            read("sibling-denied", &sibling),
            Reply::Sse(text("SIBLING-REFUSED-MARK")),
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
        .arg("read chosen file")
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(
        resumed.generations()[1]
            .to_string()
            .contains("CHOSEN-MARK-SECRET")
    );
    let sibling_run = places
        .ego(&resumed)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "workspace",
            "--mode",
            "yolo",
            "read sibling file",
        ])
        .run();
    assert_eq!(sibling_run.code, Some(0), "{}", sibling_run.stderr);
    assert!(
        !resumed
            .generations()
            .last()
            .unwrap()
            .to_string()
            .contains("OTHER-MARK-SECRET")
    );
}

/// Catches: a symlink spelling, rather than its canonical target, becomes
/// session authority and follows a replacement link on fresh resume.
#[cfg(unix)]
#[test]
fn session_root_is_bound_to_the_canonical_target_not_a_symlink() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("approved-mark.txt");
    let other = outside.path().join("unapproved-mark.txt");
    let alias = outside.path().join("alias-mark.txt");
    std::fs::write(&target, "APPROVED-MARK-SECRET").unwrap();
    std::fs::write(&other, "UNAPPROVED-MARK-SECRET").unwrap();
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let read = |id: &str, path: &std::path::Path| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"files":[{"path": path}]}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Canonical root fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            read("alias", &alias),
            Reply::Sse(text("ALIAS-MARK-PENDING")),
            Reply::Sse(text("RECORDED-MARK")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&first).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"read the alias\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    let shown = normalized(&output[start..]);
    assert!(shown.contains(target.to_str().unwrap()), "{shown}");
    input.write_all(b"2\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "ALIAS-MARK-PENDING");
    readmit_after_root_grant(&reader, &mut output, &mut input, start);
    // A turn after re-admission records the re-admitted execution
    // observation, so the next resume compares against it.
    let recorded = output.len();
    input.write_all(b"record the readmission\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, recorded, "RECORDED-MARK");
    until_prompt_from(&reader, &mut output, recorded);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&other, &alias).unwrap();

    let after = Stub::start(Plan {
        completions: vec![
            read("original-target", &target),
            Reply::Sse(text("TARGET-MARK-DONE")),
            read("changed-alias", &alias),
            Reply::Sse(text("ALIAS-MARK-DENIED")),
        ],
        ..Plan::default()
    });
    let resumed = places
        .ego(&after)
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
        .arg("read the original target")
        .run();
    assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
    assert!(
        after.generations()[1]
            .to_string()
            .contains("APPROVED-MARK-SECRET"),
        "resumed stdout: {}; stderr: {}; generation: {}",
        resumed.stdout,
        resumed.stderr,
        after.generations()[1],
    );
    let changed_alias = places
        .ego(&after)
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
        .arg("read changed alias")
        .run();
    assert_eq!(changed_alias.code, Some(0), "{}", changed_alias.stderr);
    assert!(
        !after
            .generations()
            .last()
            .unwrap()
            .to_string()
            .contains("UNAPPROVED-MARK-SECRET")
    );
}

/// Catches: same-id resume adds a PolicyEngine root but does not add the root
/// to OS process confinement, allowing built-in reads while child reads fail.
#[cfg(target_os = "macos")]
#[test]
fn resumed_session_root_reaches_a_real_sandboxed_child() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("os-mark.txt");
    std::fs::write(&target, "OS-GUARD-MARK-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let places = support::Places::new();
    places.file("AGENTS.md", "# OS root fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "ask-root",
                "read",
                &serde_json::json!({"files":[{"path": target}]}).to_string(),
            )),
            Reply::Sse(text("BEFORE-RESUME-MARK")),
        ],
        ..Plan::default()
    });
    let ego = places.ego(&first).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"read external OS fixture\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Permanent");
    input.write_all(b"2\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "BEFORE-RESUME-MARK");
    readmit_after_root_grant(&reader, &mut output, &mut input, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());

    let after = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "child-read",
                "exec",
                &serde_json::json!({"argv": ["/bin/cat", target], "timeout_ms": 5000}).to_string(),
            )),
            Reply::Sse(text("CHILD-READ-MARK-DONE")),
        ],
        ..Plan::default()
    });
    let resumed = places
        .ego(&after)
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
        .arg("run /bin/cat on the external fixture")
        .run();
    assert_eq!(resumed.code, Some(0), "{}", resumed.stderr);
    assert!(
        after.generations()[1]
            .to_string()
            .contains("OS-GUARD-MARK-SECRET"),
        "child did not read: stdout: {}; stderr: {}; generation: {}",
        resumed.stdout,
        resumed.stderr,
        after.generations()[1],
    );
}
