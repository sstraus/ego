use super::*;

// Catches: idle Enter records and clears a whitespace-only draft before admission.
#[test]
fn whitespace_only_idle_composer_keeps_draft_and_history() {
    whitespace_only_composer(false);
}

// Catches: busy Enter queues whitespace or clears it before a later real submission.
#[test]
fn whitespace_only_busy_composer_keeps_draft_and_history() {
    whitespace_only_composer(true);
}

fn whitespace_only_composer(busy: bool) {
    let (first, gate) = gated_paced_line("REGRESSION-MARKER-RUNNING");
    let stub = Stub::start(Plan {
        completions: if busy {
            vec![
                first,
                Reply::Sse(text("REGRESSION-MARKER-ANSWER")),
                Reply::Sse(text("REGRESSION-MARKER-RECALLED")),
            ]
        } else {
            vec![
                Reply::Sse(text("REGRESSION-MARKER-ANSWER")),
                Reply::Sse(text("REGRESSION-MARKER-RECALLED")),
            ]
        },
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    ego.file("AGENTS.md", "# blank composer fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    if busy {
        input.write_all(b"start gated turn\r").unwrap();
        input.flush().unwrap();
        until(&reader, &mut output, b"REGRESSION-MARKER-RUNNING");
    }

    // Paste preserves tabs and blank lines instead of invoking editor bindings.
    let blank = "  \t\n\n\u{2003}";
    input
        .write_all(format!("\x1b[200~{blank}\x1b[201~\r").as_bytes())
        .unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    let requests_after_blank = stub.generations().len();
    let valid_start = output.len();
    input.write_all(b"REGRESSION-MARKER-VALID\r").unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    if busy {
        gate.release();
    }
    until(&reader, &mut output, b"REGRESSION-MARKER-ANSWER");
    until_prompt_from(&reader, &mut output, valid_start);
    // Recall the real submission, then move back once more. A blank history
    // entry would replace this draft; an absent entry leaves it unchanged.
    input.write_all(b"\x1b[A\x1b[A").unwrap();
    input.flush().unwrap();
    drain_until_quiet(&reader, &mut output);
    let recall_start = output.len();
    input.write_all(b"-RECALL\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"REGRESSION-MARKER-RECALLED");
    until_prompt_from(&reader, &mut output, recall_start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        requests_after_blank,
        usize::from(busy),
        "blank prompt reached provider"
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 2 + usize::from(busy));
    let expected = format!("{blank}REGRESSION-MARKER-VALID");
    assert_eq!(
        count_exact_strings(&generations[usize::from(busy)]["messages"], &expected),
        1,
        "blank Enter must preserve the composer for the next real submission"
    );
    let recalled = if busy {
        "start gated turn-RECALL".to_owned()
    } else {
        format!("{expected}-RECALL")
    };
    assert_eq!(
        count_exact_strings(&generations.last().unwrap()["messages"], &recalled),
        1,
        "blank draft must not enter recall history"
    );
}
