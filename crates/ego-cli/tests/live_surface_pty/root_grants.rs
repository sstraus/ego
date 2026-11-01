use super::*;

// Catches: a Linux bwrap measurement is silently reused after a read-only root
// changes the next process's required backend to Landlock.
#[cfg(target_os = "linux")]
fn approve_linux_read_root_backend_change(places: &support::Places) {
    let denied = Stub::start(Plan::default());
    let result = places
        .ego(&denied)
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
        .arg("check the resumed root")
        .run();
    assert_eq!(result.code, Some(3), "{}", result.stderr);
    assert!(
        result
            .stderr
            .contains("backend measurement bwrap -> landlock"),
        "{}",
        result.stderr
    );
    assert!(
        denied.generations().is_empty(),
        "no effects before approval"
    );

    let approved = Stub::start(Plan {
        completions: vec![Reply::Sse(text("ROOT-READMISSION-RECORDED"))],
        ..Plan::default()
    });
    let ego = places.ego(&approved).subcommand("resume").args([
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
    until_normalized(&reader, &mut output, 0, "Allow Re-admit execution policy");
    input.write_all(b"y\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"record the backend approval\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "ROOT-READMISSION-RECORDED");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}

/// Catches: recording a root choice only in profile config, widening the live
/// PolicyEngine, or leaking the session root into a new conversation.
#[test]
fn terminal_session_root_is_denied_live_and_admitted_only_on_same_id_resume() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("policy-758.txt");
    std::fs::write(&target, "POLICY-758-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let read = |id: &str| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"path": target.to_str().unwrap()}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Root consent fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            read("denied-before"),
            Reply::Sse(text("LIVE-DENIED-758")),
            read("denied-after"),
            Reply::Sse(text("STILL-DENIED-758")),
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
    until_normalized(&reader, &mut output, start, "LIVE-DENIED-758");
    until_prompt_from(&reader, &mut output, start);
    let saved_receipt = first.generations()[1].to_string();
    assert!(saved_receipt.contains("Session root"), "{saved_receipt}");
    assert!(saved_receipt.contains("ego resume"), "{saved_receipt}");
    let next = output.len();
    input
        .write_all(b"try the same read before resume\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        next,
        "Choose Deny, Session, or Permanent",
    );
    input.write_all(b"1\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, next, "STILL-DENIED-758");
    until_prompt_from(&reader, &mut output, next);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    for generation in [1, 3] {
        assert!(
            !first.generations()[generation]
                .to_string()
                .contains("POLICY-758-SECRET"),
            "generation {generation} saw the external bytes before process restart"
        );
    }
    assert!(
        !places.home_path().join("config.toml").exists(),
        "session grant leaked into the profile"
    );

    #[cfg(target_os = "linux")]
    approve_linux_read_root_backend_change(&places);

    let resumed = Stub::start(Plan {
        completions: vec![
            read("admitted-on-resume"),
            Reply::Sse(text("REGRESSION-MARKER")),
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
        .arg("read the external file now")
        .run();
    assert_eq!(result.code, Some(0), "{}", result.stderr);
    assert!(
        resumed.generations()[1]
            .to_string()
            .contains("POLICY-758-SECRET"),
        "same-id resume did not admit the external read"
    );
    let writes = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "read-only-grant",
                "write",
                &serde_json::json!({
                    "path": target.to_str().unwrap(), "contents": "WRONG-758"
                })
                .to_string(),
            )),
            Reply::Sse(text("WRITE-REFUSED-758")),
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
        "POLICY-758-SECRET"
    );
    let other = Stub::start(Plan {
        completions: vec![
            read("different-session"),
            Reply::Sse(text("OTHER-DENIED-758")),
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
            .contains("POLICY-758-SECRET"),
        "a different session inherited the grant"
    );
}

/// Catches: after a fresh resume activates a session root, /new and /fork
/// reuse the same process-wide policy and OS guard for a different session.
#[test]
fn resumed_session_root_refuses_in_process_session_switches() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("switch-758.txt");
    std::fs::write(&target, "SWITCH-758-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let places = support::Places::new();
    places.file("AGENTS.md", "# Session switch fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "ask-root",
                "read",
                &serde_json::json!({"path": target}).to_string(),
            )),
            Reply::Sse(text("SWITCH-758-PENDING")),
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
    until_normalized(&reader, &mut output, start, "SWITCH-758-PENDING");
    until_prompt_from(&reader, &mut output, start);
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
    #[cfg(target_os = "linux")]
    {
        until_normalized(&reader, &mut output, 0, "Allow Re-admit execution policy");
        input.write_all(b"y\r").unwrap();
        input.flush().unwrap();
    }
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
    let target = outside.path().join("fork-child-758.txt");
    std::fs::write(&target, "FORK-REGRESSION-MARKER-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let places = support::Places::new();
    places.file("AGENTS.md", "# Child root fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "ask-root",
                "read",
                &serde_json::json!({"path": target}).to_string(),
            )),
            Reply::Sse(text("CHILD-ROOT-PENDING-model")),
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
    until_normalized(&reader, &mut output, start, "CHILD-ROOT-PENDING-model");
    until_prompt_from(&reader, &mut output, start);
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
            Reply::Sse(text("PARENT-TASK-DONE-758")),
        ],
        prompt_completions: vec![(
            child_prompt.into(),
            vec![
                Reply::Sse(tool_call(
                    "child-read",
                    "read",
                    &serde_json::json!({"path": target}).to_string(),
                )),
                Reply::Sse(tool_call(
                    "child-cat",
                    "exec",
                    &serde_json::json!({"argv": ["/bin/cat", target], "timeout_ms": 5000})
                        .to_string(),
                )),
                Reply::Sse(text("CHILD-ROOT-REFUSED-758")),
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
            !result.contains("FORK-REGRESSION-MARKER-SECRET"),
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
    let target = outside.path().join("cancelled-758.txt");
    std::fs::write(&target, "REGRESSION-MARKER-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let read = |id: &str| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"path": target}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Cancelled root fixture\n");
    let stub = Stub::start(Plan {
        completions: vec![
            read("cancelled-root"),
            Reply::Sse(text("session permanent approve CANCEL-758")),
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
    until_normalized(&reader, &mut output, start, "CANCEL-758");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(!places.home_path().join("config.toml").exists());
    let resumed = Stub::start(Plan {
        completions: vec![read("still-denied"), Reply::Sse(text("DENIED-758"))],
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
            .contains("REGRESSION-MARKER-SECRET")
    );
}

/// Catches: a permanent answer changes the live run, is not saved by the
/// atomic user-config transaction, or admits an unselected sibling path.
#[test]
fn permanent_root_needs_a_new_process_and_binds_the_exact_path() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("chosen-758.txt");
    let sibling = outside.path().join("other-758.txt");
    std::fs::write(&target, "CHOSEN-758-SECRET").unwrap();
    std::fs::write(&sibling, "OTHER-758-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let sibling = std::fs::canonicalize(sibling).unwrap();
    let read = |id: &str, path: &std::path::Path| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"path": path}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Permanent root fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            read("before", &target),
            Reply::Sse(text("PERMANENT-PENDING-model")),
            read("still-denied", &target),
            Reply::Sse(text("PERMANENT-STILL-DENIED-758")),
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
    until_normalized(&reader, &mut output, start, "PERMANENT-PENDING-model");
    until_prompt_from(&reader, &mut output, start);
    let saved_receipt = first.generations()[1].to_string();
    assert!(saved_receipt.contains("Permanent root"), "{saved_receipt}");
    assert!(saved_receipt.contains("ego resume"), "{saved_receipt}");
    let again = output.len();
    input
        .write_all(b"try chosen file before restart\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        again,
        "Choose Deny, Session, or Permanent",
    );
    input.write_all(b"1\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, again, "PERMANENT-STILL-DENIED-758");
    until_prompt_from(&reader, &mut output, again);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(places.home_path().join("config.toml").exists());
    assert!(
        !first.generations()[3]
            .to_string()
            .contains("CHOSEN-758-SECRET")
    );

    #[cfg(target_os = "linux")]
    approve_linux_read_root_backend_change(&places);

    let resumed = Stub::start(Plan {
        completions: vec![
            read("admitted", &target),
            Reply::Sse(text("CHOSEN-REGRESSION-MARKER")),
            read("sibling-denied", &sibling),
            Reply::Sse(text("SIBLING-REFUSED-758")),
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
            .contains("CHOSEN-758-SECRET")
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
            .contains("OTHER-758-SECRET")
    );
}

/// Catches: a symlink spelling, rather than its canonical target, becomes
/// session authority and follows a replacement link on fresh resume.
#[cfg(unix)]
#[test]
fn session_root_is_bound_to_the_canonical_target_not_a_symlink() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("approved-758.txt");
    let other = outside.path().join("unapproved-758.txt");
    let alias = outside.path().join("alias-758.txt");
    std::fs::write(&target, "APPROVED-758-SECRET").unwrap();
    std::fs::write(&other, "UNAPPROVED-758-SECRET").unwrap();
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let read = |id: &str, path: &std::path::Path| {
        Reply::Sse(tool_call(
            id,
            "read",
            &serde_json::json!({"path": path}).to_string(),
        ))
    };
    let places = support::Places::new();
    places.file("AGENTS.md", "# Canonical root fixture\n");
    let first = Stub::start(Plan {
        completions: vec![read("alias", &alias), Reply::Sse(text("ALIAS-758-PENDING"))],
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
    until_normalized(&reader, &mut output, start, "ALIAS-758-PENDING");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&other, &alias).unwrap();

    #[cfg(target_os = "linux")]
    approve_linux_read_root_backend_change(&places);

    let after = Stub::start(Plan {
        completions: vec![
            read("original-target", &target),
            Reply::Sse(text("REGRESSION-MARKER-DONE")),
            read("changed-alias", &alias),
            Reply::Sse(text("ALIAS-758-DENIED")),
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
            .contains("APPROVED-758-SECRET"),
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
            .contains("UNAPPROVED-758-SECRET")
    );
}

/// Catches: same-id resume adds a PolicyEngine root but does not add the root
/// to OS process confinement, allowing built-in reads while child reads fail.
#[cfg(target_os = "macos")]
#[test]
fn resumed_session_root_reaches_a_real_sandboxed_child() {
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("os-758.txt");
    std::fs::write(&target, "OS-GUARD-758-SECRET").unwrap();
    let target = std::fs::canonicalize(target).unwrap();
    let places = support::Places::new();
    places.file("AGENTS.md", "# OS root fixture\n");
    let first = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call(
                "ask-root",
                "read",
                &serde_json::json!({"path": target}).to_string(),
            )),
            Reply::Sse(text("BEFORE-RESUME-758")),
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
    until_normalized(&reader, &mut output, start, "BEFORE-RESUME-758");
    until_prompt_from(&reader, &mut output, start);
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
            Reply::Sse(text("CHILD-REGRESSION-MARKER-DONE")),
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
            .contains("OS-GUARD-758-SECRET"),
        "child did not read: stdout: {}; stderr: {}; generation: {}",
        resumed.stdout,
        resumed.stderr,
        after.generations()[1],
    );
}
