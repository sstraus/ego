//! Real owner input, process output, compaction publication and fresh-process resume.
use super::*;

// Catches: owner-approved helper execution is lost during compaction or resume.
#[test]
fn owner_session_exec_grant_survives_compaction_for_exact_build_path() {
    let outside = tempfile::tempdir().unwrap();
    let helper = outside.path().canonicalize().unwrap().join("built-helper");
    std::fs::copy("/bin/echo", &helper).unwrap();
    // A relocated Apple platform binary is killed by AMFI even outside the
    // sandbox. Make this fixture an ordinary locally signed helper first.
    assert!(
        std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&helper)
            .status()
            .unwrap()
            .success()
    );
    let args = serde_json::json!({"argv": [helper, "HELPER-RAN"], "timeout_ms": 5000}).to_string();
    let call = |id| Reply::Sse(tool_call(id, "exec", &args));
    let checkpoint = "# Objective\nContinue the helper task.\n# Binding constraints and preferences\nNone.\n# Completed work and evidence\nOwner granted helper execution.\n# Work in progress\nRun the helper.\n# Decisions and rationale\nExact owner grant.\n# Blockers and uncertainty\nNone.\n# Ordered next steps\nRun the helper.\n# Critical references\nNone.";
    let stub = Stub::start(Plan {
        completions: vec![
            call("refused"),
            Reply::Sse(text("GRANT-SAVED")),
            Reply::Sse(text(checkpoint)),
            call("compacted"),
            Reply::Sse(text("COMPACT-DONE")),
            call("resumed"),
            Reply::Sse(text("RESUME-DONE")),
        ],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# Session grant fixture\n");
    std::fs::write(places.home_path().join("config.toml"),
        "model='ollama/qwen3'\nthinking='off'\nmode='default'\nsandbox='workspace'\n[permissions]\nallow=['exec(*)']\n").unwrap();
    let ego = places
        .ego(&stub)
        .conversing()
        .args(["--session-id", support::FIXTURE_SESSION_ID]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 160, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"run the exact helper\r").unwrap();
    input.flush().unwrap();
    // A baseline without exec grants reaches the final answer instead of a modal.
    // Shut down before asserting so RED leaves no child process behind.
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let offered = loop {
        let shown = normalized(&output[start..]);
        if shown.contains("for this session") {
            break true;
        }
        if shown.contains("GRANT-SAVED") {
            break false;
        }
        let chunk = reader
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        output.extend(chunk);
    };
    if !offered {
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        panic!(
            "exact built helper had no owner exec approval: {}",
            normalized(&output)
        );
    }
    input.write_all(b"y").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "GRANT-SAVED");
    assert!(
        !tool_output(&stub.generations()[1]).contains("HELPER-RAN"),
        "approval replayed the helper"
    );
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let compact = output.len();
    input.write_all(b"/compact\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, compact, "compacted session");
    until_successor_ready(&reader, &mut output, compact, "compact", &stub);
    drain_until_quiet(&reader, &mut output);
    let next = output.len();
    input.write_all(b"run helper after compaction\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, next, "COMPACT-DONE");
    assert!(
        tool_output(&stub.generations()[4]).contains("HELPER-RAN"),
        "{}",
        stub.generations()[4]
    );
    until_prompt_from(&reader, &mut output, next);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let listing = places
        .ego(&stub)
        .subcommand("ls")
        .args(["--output", "json"])
        .run()
        .report();
    let target = listing["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] != support::FIXTURE_SESSION_ID)
        .unwrap();
    let resumed = places
        .ego(&stub)
        .subcommand("resume")
        .args([target["id"].as_str().unwrap()]);
    let (session, mut input, reader) = PtySession::start(&resumed, 40, 160, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let next = output.len();
    input.write_all(b"run helper after resume\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, next, "RESUME-DONE");
    assert!(
        tool_output(&stub.generations()[6]).contains("HELPER-RAN"),
        "{}",
        stub.generations()[6]
    );
    until_prompt_from(&reader, &mut output, next);
    // Catches: the owner surface hides exec grants or refuses to revoke them.
    let listed = output.len();
    input.write_all(b"/config roots\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, listed, "/config roots revoke");
    let targets = normalized(&output[listed..]);
    assert!(
        targets
            .lines()
            .any(|line| line.contains(helper.to_str().unwrap()) && line.contains("exec")),
        "{targets}"
    );
    until_prompt_from(&reader, &mut output, listed);
    let revoked = output.len();
    input
        .write_all(format!("/config roots revoke {}\r", helper.display()).as_bytes())
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, revoked, "Root revoked");
    until_prompt_from(&reader, &mut output, revoked);
    let after = output.len();
    input.write_all(b"/config roots\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, after, "/config roots revoke");
    // Revocation rebuilds the session and replays its transcript, which still
    // shows the helper's exec row; judge only the listing itself.
    let shown = normalized(&output[after..]);
    let targets = shown
        .rsplit_once("session targets:")
        .map(|(_, listing)| listing)
        .unwrap_or_else(|| panic!("no listing: {shown}"));
    assert!(!targets.contains(helper.to_str().unwrap()), "{targets}");
    until_prompt_from(&reader, &mut output, after);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}

fn tool_output(request: &serde_json::Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
