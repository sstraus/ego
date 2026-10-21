use super::*;

// Catches: successful shell progress stacks instead of repainting one row,
// or replacement erases a failed command or the last command at turn end.
#[test]
fn consecutive_shell_rows_replace_in_place_and_retain_failure() {
    shell_rows_contract(false);
}

// Catches: compact presentation leaks into explicitly append-only output.
#[test]
fn plain_shell_rows_remain_append_only() {
    shell_rows_contract(true);
}

fn shell_rows_contract(plain: bool) {
    let gate = ReleaseGate::new();
    let calls = [
        r#"{"argv":["sh","-c",": SHELL-FIRST"]}"#,
        r#"{"argv":["sh","-c",": SHELL-SECOND"]}"#,
        r#"{"argv":["sh","-c",": SHELL-FAIL; exit 7"]}"#,
        r#"{"argv":["sh","-c",": SHELL-THIRD"]}"#,
        r#"{"argv":["sh","-c",": SHELL-LAST"]}"#,
    ];
    let mut completions: Vec<_> = calls
        .iter()
        .enumerate()
        .map(|(index, args)| Reply::Sse(tool_call(&format!("shell-{index}"), "exec", args)))
        .collect();
    completions.push(Reply::GatedPaced {
        head: String::new(),
        tail: text("SHELL-DONE"),
        gate: Arc::clone(&gate),
    });
    let stub = Stub::start(Plan {
        completions,
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--mode",
            "yolo",
            "--sandbox",
            "off",
        ])
        .env("TERM", "xterm-256color");
    let ego = if plain { ego.arg("--plain") } else { ego };
    ego.file("AGENTS.md", "# shell row fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 50, 160, Some((50, 160)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"run commands\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "SHELL-LAST");
    drain_until_quiet(&reader, &mut output);
    let live = screen_rows(&output, 50, 160).join("\n");
    let completion_start = output.len();
    gate.release();
    until_normalized(&reader, &mut output, completion_start, "SHELL-DONE");
    until_prompt_from(&reader, &mut output, completion_start);
    drain_until_quiet(&reader, &mut output);
    let settled = screen_rows(&output, 50, 160).join("\n");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    // A failed tool does not fail the recovered turn.
    assert!(session.quit(input).success());
    for screen in [&live, &settled] {
        assert!(
            screen.contains("SHELL-FAIL") && screen.contains("exit 7"),
            "failure lost: {screen}"
        );
        assert!(
            screen.contains("SHELL-SECOND"),
            "boundary receipt lost: {screen}"
        );
        assert!(screen.contains("SHELL-LAST"), "last command lost: {screen}");
        for replaced in ["SHELL-FIRST", "SHELL-THIRD"] {
            assert_eq!(
                screen.contains(replaced),
                plain,
                "replacement/append contract for {replaced}: {screen}"
            );
        }
    }
}

// Catches: a successful command's output is discarded as replaceable progress,
// or a later command replaces the receipt immediately before that output.
#[test]
fn shell_output_breaks_the_replaceable_run() {
    let screens = tool_turn(
        120,
        &[],
        &[
            ("exec", r#"{"argv":["sh","-c",": BEFORE-OUTPUT"]}"#),
            ("exec", r#"{"argv":["printf","OUTPUT-IS-EVIDENCE\n"]}"#),
            ("exec", r#"{"argv":["sh","-c",": AFTER-OUTPUT"]}"#),
        ],
        false,
    );
    let shown = screens.after_turn.join("\n");
    assert_eq!(tool_rows(&screens.after_turn).len(), 3, "{shown}");
    assert!(
        shown
            .lines()
            .any(|line| line.trim() == "OUTPUT-IS-EVIDENCE"),
        "output lost: {shown}"
    );
    assert!(
        shown.contains("BEFORE-OUTPUT") && shown.contains("AFTER-OUTPUT"),
        "boundary receipt lost: {shown}"
    );
}
