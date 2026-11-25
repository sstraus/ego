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
            assert!(!normalized(&output[before..]).contains("Choose MCP server or action"));
            assert!(stub.generations().is_empty());
        }
        input.write_all(accept).expect("accept selection");
        input.flush().expect("flush");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            until_normalized_without_new_generation(
                &reader,
                &mut output,
                start,
                "Choose MCP server or action",
                &stub,
                0,
            );
            input.write_all(b"\r").expect("show server status");
            input.flush().expect("flush");
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
    selected_skill_completion(false);
}

// Catches color missing from the real menu or the styled badge leaking into dispatch.
#[test]
fn selected_skill_completion_paints_only_the_badge() {
    selected_skill_completion(true);
}

fn selected_skill_completion(colored: bool) {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(text(r#""clear""#)),
            Reply::Sse(text("SELECTED-SKILL-REPLY")),
        ],
        ..Plan::default()
    });
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    let ego = if colored {
        ego.env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("COLORFGBG", "0;15")
    } else {
        ego
    };
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
    if colored {
        until(&reader, &mut output, b"\x1b[38;2;75;96;120m[project]\x1b[");
    } else {
        until_normalized(&reader, &mut output, start, "[PROJECT] .EGO");
    }
    input.write_all(b"\r").expect("accept skill");
    input.flush().expect("flush");
    until(&reader, &mut output, b"SELECTED-SKILL-REPLY");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("flush");
    assert!(session.quit(input).success());
    let generations = stub.generations();
    assert_eq!(generations.len(), 2, "one review and one skill invocation");
    assert!(
        generations[0]
            .to_string()
            .contains("untrusted_skill_evidence")
    );
    let request = generations[1].to_string();
    assert!(request.contains("Admitted skill instruction"), "{request}");
    assert!(
        request.contains("SELECTED-SKILL-INSTRUCTION-537"),
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

// Catches: an exact command closes the menu permanently, or reopening restores an erased character.
#[test]
fn backspace_after_an_exact_slash_command_reopens_the_completion() {
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
    // With no TERM, Reedline marks the selected plain menu row in uppercase.
    input.write_all(b"/hooks").expect("prefix");
    input.flush().expect("flush");
    drain_until_quiet(&reader, &mut output);
    let before = output.len();
    input.write_all(b"\x7f").expect("erase to /hook");
    input.flush().expect("flush");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        until_normalized(&reader, &mut output, before, "/HOOKS");
        drain_until_quiet(&reader, &mut output);
        let before = output.len();
        input.write_all(b"\x7f\x7f\x7f").expect("erase to /h");
        input.flush().expect("flush");
        until_normalized(&reader, &mut output, before, "/HELP");
        // A trailing typed key must not re-enable quick insertion for earlier
        // Backspaces in the same terminal input burst.
        input.write_all(b"\x15/hooks").expect("exact command again");
        input.flush().expect("flush");
        drain_until_quiet(&reader, &mut output);
        let before = output.len();
        input
            .write_all(b"\x7f\x7f\x7f\x7fe")
            .expect("erase to /h then type e");
        input.flush().expect("flush");
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

// Catches: with persistent menus, Delete or Left/Right at the end of a /-prefixed draft
// closes the menu or moves the cursor, so Enter submits the partial "/mc" as unknown text.
#[test]
fn delete_and_horizontal_arrows_keep_the_open_menu_acceptable() {
    let stub = Stub::start(Plan::default());
    let ego =
        Ego::against(&stub)
            .conversing()
            .args(["--model", "ollama/qwen3", "--sandbox", "off"]);
    ego.file("AGENTS.md", "# Slash delete fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust");
    input.flush().expect("flush");
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/mc").expect("prefix");
    input.flush().expect("flush");
    until_normalized(&reader, &mut output, start, "/MCP");
    drain_until_quiet(&reader, &mut output);
    input
        .write_all(b"\x1b[3~\x1b[D\x1b[C\r")
        .expect("delete, left, right, accept");
    input.flush().expect("flush");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        until_normalized_without_new_generation(
            &reader,
            &mut output,
            start,
            "Choose MCP server or action",
            &stub,
            0,
        );
        input.write_all(b"\r").expect("show server status");
        input.flush().expect("flush");
        until_normalized_without_new_generation(&reader, &mut output, start, "mcp:", &stub, 0);
        until_prompt_from(&reader, &mut output, start);
        drain_until_quiet(&reader, &mut output);
        let text = normalized(&output[start..]);
        assert_eq!(text.matches("mcp:").count(), 1, "{text}");
        assert!(!text.contains("Unknown slash command"), "{text}");
    }));
    input.write_all(b"\x15/quit\r").expect("exit");
    input.flush().expect("flush");
    session.quit(input);
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
