use super::*;

// Catches: a non-interactive prompt beginning with `!` is mistaken for a
// trusted editor submission and executes a local process.
#[test]
fn headless_bang_remains_a_model_prompt() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("HEADLESS-BANG-MARK"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# Headless shell fixture\n");
    let run = places
        .ego(&stub)
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .arg("! printf WRONG-MARK > headless-marker")
        .run();
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert!(!places.workspace_path().join("headless-marker").exists());
    let generations = stub.generations();
    assert_eq!(generations.len(), 1);
    assert!(
        generations[0]
            .to_string()
            .contains("! printf WRONG-MARK > headless-marker")
    );
}

// Catches: a terminal-entered shell escape is forwarded as a model prompt or
// executed without the shell's real observable filesystem effect.
#[test]
fn terminal_bang_executes_locally_without_a_model_turn() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("UNEXPECTED-MODEL-TURN"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# Shell escape fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "workspace",
        "--mode",
        "plan",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input
        .write_all(b"! printf BANG-MARK > shell-marker\r")
        .unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        std::fs::read_to_string(places.workspace_path().join("shell-marker")).unwrap(),
        "BANG-MARK"
    );
    assert!(
        stub.generations().is_empty(),
        "shell escape bought a model turn"
    );
}

// Catches: a busy Enter or Ctrl+Enter puts a shell escape in the model queue
// instead of refusing it and keeping the draft for an idle submission.
#[test]
fn busy_bang_is_refused_and_stays_in_the_editor_until_idle() {
    for (label, submit) in [
        ("enter", b"\r".as_slice()),
        ("ctrl-enter", b"\x1b[13;5u".as_slice()),
    ] {
        let (reply, gate) = gated_paced_line("BANG-MARK-BUSY");
        let stub = Stub::start(Plan {
            completions: vec![reply, Reply::Sse(text("QUEUED-MARK-UNEXPECTED"))],
            ..Plan::default()
        });
        let places = support::Places::new();
        places.file("AGENTS.md", "# Busy shell fixture\n");
        let ego = places.ego(&stub).conversing().args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ]);
        let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        let trust = output.len();
        input.write_all(b"yes\r").unwrap();
        input.flush().unwrap();
        until_prompt_from(&reader, &mut output, trust);
        input.write_all(b"hold turn\r").unwrap();
        input.flush().unwrap();
        until(&reader, &mut output, b"BANG-MARK-BUSY");
        let start = output.len();
        input
            .write_all(b"! printf BANG-MARK-IDLE > busy-marker")
            .unwrap();
        input.write_all(submit).unwrap();
        input.flush().unwrap();
        until_normalized_without_new_generation(
            &reader,
            &mut output,
            start,
            "shell escapes require an idle terminal",
            &stub,
            1,
        );
        assert!(
            !places.workspace_path().join("busy-marker").exists(),
            "{label}"
        );
        gate.release();
        until_prompt_from(&reader, &mut output, start);
        let idle = output.len();
        input.write_all(b"\r").unwrap();
        input.flush().unwrap();
        until_prompt_from(&reader, &mut output, idle);
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success(), "{label}");
        assert_eq!(
            std::fs::read_to_string(places.workspace_path().join("busy-marker")).unwrap(),
            "BANG-MARK-IDLE",
            "{label}: the refused editor draft was lost",
        );
        assert_eq!(
            stub.generations().len(),
            1,
            "{label}: busy bang reached the model"
        );
    }
}

// Catches: user-origin shell admission bypasses an explicit deny rule while
// bypassing only the model-approval and Plan-mode gates.
#[test]
fn terminal_bang_still_obeys_a_deny_rule() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# Shell denial fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "plan",
        "--deny",
        "exec(/bin/sh *)",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input
        .write_all(b"! printf WRONG-MARK > denied-marker\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "denied by");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    let _ = session.quit(input);
    assert!(!places.workspace_path().join("denied-marker").exists());
    assert!(stub.generations().is_empty());
}

// Catches: provider-produced text starting with `!` is replayed through the
// editor command path and executes a process without a terminal submission.
#[test]
fn model_text_containing_bang_cannot_execute_a_shell_command() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("! printf ATTACK-MARK > model-marker"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# Shell escape fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "workspace",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let trust = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, trust);
    let start = output.len();
    input.write_all(b"describe this\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "ATTACK-MARK");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(!places.workspace_path().join("model-marker").exists());
    assert_eq!(stub.generations().len(), 1);
}
