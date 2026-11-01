use super::*;

// Catches: Option+O takes the output lock while finish holds it waiting for
// the bounded printer that only the keyboard owner can drain.
#[test]
fn expand_during_turn_finish_does_not_deadlock_terminal() {
    let (first, gate) = gated_paced_line("expand-visible-streams-STREAMING");
    let Reply::GatedPaced { head, .. } = first else {
        unreachable!();
    };
    let answer: String = (0..2000)
        .map(|row| format!("expand-visible-streams-ROW-{row:04}\n"))
        .collect();
    let stub = Stub::start(Plan {
        completions: vec![Reply::GatedPaced {
            head,
            tail: text(&answer),
            gate: Arc::clone(&gate),
        }],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# expand finish fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"stream output\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "expand-visible-streams-STREAMING");
    let start = output.len();
    gate.release();
    // Keep real key events arriving across the row pump's finish barrier.
    // No sleeps or unbounded wait: a frozen owner is killed by session.quit.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        until_normalized(
            &reader,
            &mut output,
            start,
            "expand-visible-streams-ROW-0000",
        );
        input.write_all(&b"\x1bo".repeat(32)).unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "no tool call to expand yet");
        until_normalized(
            &reader,
            &mut output,
            start,
            "expand-visible-streams-ROW-1999",
        );
        input.write_all(b"RESPONSIVE742").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "RESPONSIVE742");
    }));
    if result.is_err() {
        session
            .child
            .clone_killer()
            .kill()
            .expect("kill frozen fixture");
    } else {
        input.write_all(b"\x03").unwrap();
        input.flush().unwrap();
        drain_until_quiet(&reader, &mut output);
        input.write_all(b"/expand\r").unwrap();
        input.flush().unwrap();
        let slash_start = output.len();
        until_normalized(
            &reader,
            &mut output,
            slash_start,
            "no tool call to expand yet",
        );
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
    }
    let status = session.quit(input);
    if let Err(failure) = result {
        std::panic::resume_unwind(failure);
    }
    assert!(status.success(), "REPL exit: {status:?}");
    assert_eq!(stub.generations().len(), 1, "expand must remain local");
}
