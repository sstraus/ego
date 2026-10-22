use super::*;

// Catches: accepting an active slash menu consumes Enter without dispatching its command.
#[test]
fn selected_slash_completion_enter_runs_once() {
    selected_completion(b"\r", false);
}

// Catches: resolving host submission also makes Tab execute instead of only insert.
#[test]
fn selected_slash_completion_tab_only_inserts() {
    selected_completion(b"\r", true);
}

// Catches: a navigated selection bypasses completion resolution at host submission.
#[test]
fn selected_slash_completion_arrows_then_enter_runs_once() {
    selected_completion(b"\x1b[B\x1b[A\r", false);
}

fn selected_completion(accept: &[u8], tab_first: bool) {
    for (rows, cols) in [(40, 120), (24, 80)] {
        let stub = Stub::start(Plan::default());
        let ego =
            Ego::against(&stub)
                .conversing()
                .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
        ego.file("AGENTS.md", "# Slash completion fixture\n");
        let (session, mut input, reader) = PtySession::start(&ego, rows, cols, None);
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        let start = output.len();
        input.write_all(b"yes\r").expect("trust");
        input.flush().expect("flush");
        until_prompt_from(&reader, &mut output, start);
        let start = output.len();
        input.write_all(b"/mc").expect("prefix");
        input.flush().expect("flush");
        // With no TERM, Reedline marks the selected plain menu row in uppercase.
        until_normalized(&reader, &mut output, start, "/MCP");
        drain_until_quiet(&reader, &mut output);
        if tab_first {
            let before = output.len();
            input.write_all(b"\t").expect("insert selection");
            input.flush().expect("flush");
            drain_until_quiet(&reader, &mut output);
            assert!(!normalized(&output[before..]).contains("mcp:"));
            assert!(stub.generations().is_empty());
        }
        input.write_all(accept).expect("accept selection");
        input.flush().expect("flush");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            until_normalized_without_new_generation(&reader, &mut output, start, "mcp:", &stub, 0);
            until_prompt_from(&reader, &mut output, start);
            drain_until_quiet(&reader, &mut output);
            assert_eq!(normalized(&output[start..]).matches("mcp:").count(), 1);
        }));
        record_slash_capture(&output, rows, cols);
        input.write_all(b"\x15/quit\r").expect("exit");
        input.flush().expect("flush");
        session.quit(input);
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}

// Catches: accepting a skill completion submits literal slash text or invokes it twice.
#[test]
fn selected_skill_completion_enter_delivers_one_admitted_instruction() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("SELECTED-SKILL-REPLY"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# Selected skill fixture\n");
    ego.file(
        ".ego/skills/zebra-completion/SKILL.md",
        "---\nname: zebra-completion\ndescription: Selected skill consumer\nuser-invocable: true\ndisable-model-invocation: true\n---\nSELECTED-SKILL-INSTRUCTION-537\n",
    );
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust");
    input.flush().expect("flush");
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/zebra-c").expect("skill prefix");
    input.flush().expect("flush");
    // With no TERM, Reedline marks the selected plain menu row in uppercase.
    until_normalized(&reader, &mut output, start, "/ZEBRA-COMPLETION");
    input.write_all(b"\r").expect("accept skill");
    input.flush().expect("flush");
    until(&reader, &mut output, b"SELECTED-SKILL-REPLY");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("flush");
    assert!(session.quit(input).success());
    let generations = stub.generations();
    assert_eq!(generations.len(), 1, "one selected skill invocation");
    let request = generations[0].to_string();
    assert!(request.contains("Admitted skill instruction"), "{request}");
    assert!(
        request.contains("REGRESSION-MARKER"),
        "{request}"
    );
}

// Catches: Backspace inside a /-prefixed draft clears the completion state instead of re-filtering.
#[test]
fn backspace_in_partial_slash_command_refilters_the_completion() {
    let stub = Stub::start(Plan::default());
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# Slash backspace fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust");
    input.flush().expect("flush");
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    // With no TERM, Reedline marks the selected plain menu row in uppercase.
    input.write_all(b"/hoo").expect("prefix");
    input.flush().expect("flush");
    until_normalized(&reader, &mut output, start, "/HOOKS");
    drain_until_quiet(&reader, &mut output);
    let before = output.len();
    input.write_all(b"\x7f\x7f").expect("erase to /h");
    input.flush().expect("flush");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        until_normalized(&reader, &mut output, before, "/HELP");
    }));
    input.write_all(b"\x15/quit\r").expect("exit");
    input.flush().expect("flush");
    session.quit(input);
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// Catches: Backspace that deletes the leading / leaves a stale completion list open.
#[test]
fn backspace_over_the_leading_slash_closes_the_completion() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("PLAIN-PROMPT-REPLY"))],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# Slash backspace fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust");
    input.flush().expect("flush");
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/h").expect("prefix");
    input.flush().expect("flush");
    until_normalized(&reader, &mut output, start, "/HELP");
    drain_until_quiet(&reader, &mut output);
    input
        .write_all(b"\x7f\x7fhi\r")
        .expect("erase /h, type prompt");
    input.flush().expect("flush");
    until(&reader, &mut output, b"PLAIN-PROMPT-REPLY");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("flush");
    assert!(session.quit(input).success());
    let generations = stub.generations();
    assert_eq!(
        generations.len(),
        1,
        "one plain prompt, no accepted menu row"
    );
    assert!(generations[0].to_string().contains("hi"));
}
