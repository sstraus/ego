use super::*;

const CHECKPOINT: &str = "# Objective\nContinue the task.\n# Binding constraints and preferences\nNone.\n# Completed work and evidence\nFirst request.\n# Work in progress\nFinish task.\n# Decisions and rationale\nNone.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nFinish task.\n# Critical references\nNone.";

// Catches: partial provider output closes the interactive process or is replayed automatically.
#[test]
fn uncertain_partial_provider_output_keeps_the_pty_open_for_a_fresh_prompt() {
    let partial = text("PARTIAL-MARK")
        .split("\n\n")
        .take(2)
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\ndata: {\"error\":{\"message\":\"temporarily unavailable\",\"type\":\"api_error\"}}\n\n";
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(partial), Reply::Sse(text("FRESH-MARK"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# uncertain provider fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--retry",
        "0",
    ]);
    let (mut session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"start this task\r").unwrap();
    input.flush().unwrap();
    until_notice_prose(&reader, &mut output, start, "temporarily unavailable");
    until_prompt_from(&reader, &mut output, start);
    assert!(session.child.try_wait().unwrap().is_none());
    assert_eq!(
        stub.generations().len(),
        1,
        "partial output must not be replayed"
    );
    let start = output.len();
    input.write_all(b"a fresh request\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "FRESH-MARK");
    assert!(session.child.try_wait().unwrap().is_none());
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(stub.generations().len(), 2);
}

// Catches: a provider overflow without a usage measurement never schedules recovery.
#[test]
fn provider_window_overflow_compacts_without_measured_pressure() {
    recover_window(Reply::Sse("data: {\"error\":{\"message\":\"maximum context length exceeded\",\"code\":\"context_length_exceeded\"}}\n\ndata: [DONE]\n\n".into()), None);
}

// Catches: measured exhaustion is refused again instead of reaching the checkpoint owner.
#[test]
fn measured_window_overflow_compacts_and_keeps_the_pty_alive() {
    recover_window(Reply::Sse(support::costing("OVER-WINDOW", 280000, 4)), None);
}

// Catches: permanent checkpoint failure kills the interactive process instead of offering retry.
#[test]
fn failed_window_compaction_keeps_the_pty_open_for_retry() {
    recover_window(
        Reply::Sse(support::costing("OVER-WINDOW", 280000, 4)),
        Some(Reply::Status(400)),
    );
}

fn recover_window(first: Reply, checkpoint_failure: Option<Reply>) {
    let fail_checkpoint = checkpoint_failure.is_some();
    let mut completions = vec![first];
    if let Some(failure) = checkpoint_failure {
        completions.push(failure);
    }
    completions.extend([
        Reply::Sse(text(CHECKPOINT)),
        Reply::Sse(text("RECOVERED-MARK")),
    ]);
    let stub = Stub::start(Plan {
        completions,
        context_length: Some(272000),
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# window recovery fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "--retry",
        "0",
    ]);
    let (mut session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"finish this task\r").unwrap();
    input.flush().unwrap();
    if fail_checkpoint {
        until_notice_prose(&reader, &mut output, start, "Retry with '/compact'");
        assert!(
            session.child.try_wait().unwrap().is_none(),
            "failed recovery must stay alive"
        );
        assert_eq!(stub.generations().len(), 2, "failed recovery must not spin");
        input.write_all(b"/compact\r").unwrap();
        input.flush().unwrap();
        until_successor_receipt_ready(&reader, &mut output, start, "compact", &stub);
        input.write_all(b"continue\r").unwrap();
        input.flush().unwrap();
    }
    until_normalized(&reader, &mut output, start, "RECOVERED-MARK");
    assert!(
        session.child.try_wait().unwrap().is_none(),
        "recovery must keep the process alive"
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let requests = stub.generations();
    assert_eq!(requests.len(), if fail_checkpoint { 4 } else { 3 });
    assert!(
        requests
            .last()
            .unwrap()
            .to_string()
            .contains("Continue the task.")
    );
}

// Catches: a successor that still exceeds the provider window starts another
// automatic checkpoint instead of returning control after unsuccessful recovery.
#[test]
fn repeated_provider_overflow_after_checkpoint_returns_control_without_recompacting() {
    let overflow = "data: {\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"window still exceeded\"}}\n\n";
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(overflow.to_owned()),
            Reply::Sse(text(CHECKPOINT)),
            Reply::Sse(overflow.to_owned()),
            Reply::Sse(text(CHECKPOINT)),
            Reply::Sse(text("UNREQUESTED-SECOND-RECOVERY")),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# repeated provider overflow fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "--retry",
        "0",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"finish this task\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Resuming the task automatically",
    );
    let resumed = start + find(&output[start..], b"Resuming the task automatically");
    until_normalized(&reader, &mut output, resumed, "window still exceeded");
    until_prompt_from(&reader, &mut output, resumed);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    let status = session.quit(input);
    assert_eq!(
        stub.generations().len(),
        3,
        "a failed successor must yield control instead of buying another checkpoint: {}",
        normalized(&output)
    );
    assert!(
        status.success(),
        "explicit quit must remain normal: {status:?}"
    );
}

// Catches: a manual retry clears the failed-unmeasured-overflow latch and
// silently starts a second checkpoint after the requested retry also fails.
#[test]
fn manual_retry_after_unmeasured_overflow_failure_does_not_retry_again_automatically() {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse("data: {\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"window exceeded\"}}\n\n".to_owned()),
            Reply::Status(400),
            Reply::Status(400),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# manual retry after unknown usage fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--session-id",
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
        "--retry",
        "0",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"finish this task\r").unwrap();
    input.flush().unwrap();
    until_notice_prose(&reader, &mut output, start, "Retry with '/compact'");
    until_prompt_from(&reader, &mut output, start);
    let retry = output.len();
    input.write_all(b"/compact\r").unwrap();
    input.flush().unwrap();
    until_notice_prose(&reader, &mut output, retry, "Retry with '/compact'");
    until_prompt_from(&reader, &mut output, retry);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    let status = session.quit(input);
    assert_eq!(
        stub.generations().len(),
        3,
        "one initial request, one automatic checkpoint and one explicit retry: {}",
        normalized(&output)
    );
    assert!(
        status.success(),
        "explicit quit must remain normal: {status:?}"
    );
}
