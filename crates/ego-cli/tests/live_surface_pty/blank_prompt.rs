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
                Reply::Sse(text("REGRESSION-MARKER-ANCHOR")),
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
    let anchor = if busy {
        "start gated turn"
    } else {
        "history anchor"
    };
    let anchor_start = output.len();
    input.write_all(format!("{anchor}\r").as_bytes()).unwrap();
    input.flush().unwrap();
    if busy {
        until(&reader, &mut output, b"REGRESSION-MARKER-RUNNING");
    } else {
        until(&reader, &mut output, b"REGRESSION-MARKER-ANCHOR");
        until_prompt_from(&reader, &mut output, anchor_start);
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
    // History recall rests at the first line end; Alt+> moves to the full
    // multiline buffer end before appending the observable recall suffix.
    input.write_all(b"\x1b[A\x1b[A\x1b>").unwrap();
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
    assert_eq!(requests_after_blank, 1, "blank prompt reached provider");
    let generations = stub.generations();
    assert_eq!(generations.len(), 3);
    let expected = format!("{blank}REGRESSION-MARKER-VALID");
    assert_eq!(
        count_exact_strings(&generations[1]["messages"], &expected),
        1,
        "blank Enter must preserve the composer for the next real submission"
    );
    // Use an explicit previous user prompt: startup trust answers may also
    // be in editor history, so the oldest entry is not an empty-history oracle.
    let recalled = format!("{anchor}-RECALL");
    assert_eq!(
        count_exact_strings(&generations.last().unwrap()["messages"], &recalled),
        1,
        "blank draft must not enter recall history: {:?}",
        generations.last().unwrap()["messages"]
    );
}
