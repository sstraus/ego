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

// Catches: the live foreground preview arrives late or duplicates output.
#[test]
fn running_build_updates_one_terminal_row_before_completion() {
    let gate = ReleaseGate::new();
    let args = r#"{"argv":["sh","fake-build.sh"]}"#;
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Sse(tool_call("live-build", "exec", args)),
            Reply::GatedPaced {
                head: String::new(),
                tail: text("BUILD-DONE"),
                gate: Arc::clone(&gate),
            },
        ],
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
    ego.file("AGENTS.md", "# live build fixture\n");
    ego.file(
        "fake-build.sh",
        concat!(
            "printf 'BUILD-ONE\\n'; while [ ! -f release-first ]; do sleep 0.05; done\n",
            "printf 'BUILD-TWO\\n'; while [ ! -f release-second ]; do sleep 0.05; done\n",
        ),
    );
    let (session, mut input, reader) = PtySession::start(&ego, 50, 160, Some((50, 160)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"run build\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "BUILD-ONE");
    let first = screen_rows(&output, 50, 160);
    assert_live_build_preview(&first, &["BUILD-ONE"]);
    let offset = output.len();
    ego.file("release-first", "");
    until_normalized(&reader, &mut output, offset, "BUILD-TWO");
    let second = screen_rows(&output, 50, 160);
    assert_live_build_preview(&second, &["BUILD-ONE", "BUILD-TWO"]);
    let offset = output.len();
    ego.file("release-second", "");
    gate.release();
    until_normalized(&reader, &mut output, offset, "BUILD-DONE");
    until_prompt_from(&reader, &mut output, offset);
    let settled = screen_rows(&output, 50, 160).join("\n");
    assert!(
        settled.contains("BUILD-ONE") && settled.contains("BUILD-TWO"),
        "{settled}"
    );
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}

fn assert_live_build_preview(rows: &[String], expected: &[&str]) {
    let working: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains("Working") && row.contains("$ sh"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(working.len(), 1, "{rows:?}");
    let output: Vec<_> = rows.iter().map(|row| row.trim()).collect();
    assert_eq!(
        &output[working[0] + 1..][..expected.len()],
        expected,
        "{rows:?}"
    );
    let build_lines: Vec<_> = output
        .into_iter()
        .filter(|row| row.starts_with("BUILD-"))
        .collect();
    assert_eq!(build_lines, expected, "{rows:?}");
}
