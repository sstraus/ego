use super::*;

// Catches: finish waits on the printer while its terminal owner is blocked
// delivering the second unread Line to a capacity-one driver mailbox.
#[test]
fn turn_finish_drains_typeahead_without_pending_answer() {
    finish_with_typeahead(false);
}

// Catches: pending answer rows fill the printer while queued input prevents
// the terminal owner from draining it, deadlocking the output-pump barrier.
#[test]
fn turn_finish_drains_typeahead_with_pending_answer() {
    finish_with_typeahead(true);
}

fn finish_with_typeahead(pending_answer: bool) {
    let (first, gate) = gated_paced_line("REGRESSION-MARKER-RUNNING");
    let Reply::GatedPaced {
        head,
        tail,
        gate: _,
    } = first
    else {
        unreachable!();
    };
    let tail = if pending_answer {
        text("REGRESSION-MARKER-ROW-A\nFINISH-463-ROW-B\nFINISH-463-ROW-C\n")
    } else {
        tail
    };
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::GatedPaced {
                head,
                tail,
                gate: Arc::clone(&gate),
            },
            Reply::Sse(text("REGRESSION-MARKER-RECOVERED")),
        ],
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
    ego.file("AGENTS.md", "# turn finish fixture\n");
    // Allow replay against an explicitly selected historical executable.
    let command = match std::env::var_os("EGO_REGRESSION_BINARY") {
        Some(binary) => ego.pty_command_with_program(binary),
        None => ego.pty_command(),
    };
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(command).expect("spawn");
    let (mut input, responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream(pair.master.try_clone_reader().expect("reader"), responder);
    let session = PtySession {
        child,
        pair,
        reader_join,
        writer_join,
    };
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"start gated turn\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"REGRESSION-MARKER-RUNNING");

    // /tasks is delivered to the driver even while generation is running,
    // unlike normal busy submissions which use the turn's durable ingress.
    // Three lines exceed the real mailbox capacity (one) without a test seam.
    // TEST-CHANGE: read-only receipts now arrive during generation. Capture
    // the whole burst, not only output after releasing the provider gate.
    let receipts_start = output.len();
    input
        .write_all(b"/tasks\r/tasks invalid\r/tasks invalid extra\r")
        .unwrap();
    input.flush().unwrap();
    // Busy submissions clear the editor instead of echoing accepted text.
    // Let the real owner consume the burst before ending the generation.
    drain_until_quiet(&reader, &mut output);
    let finished = output.len();
    gate.release();
    until_normalized(
        &reader,
        &mut output,
        receipts_start,
        "usage: /tasks [ID | cancel ID]",
    );
    // TEST-CHANGE: keep a separate post-gate prompt barrier and recovery
    // checks even when all three receipts were printed before gate release.
    until_prompt_from(&reader, &mut output, finished);
    let receipts = normalized(&output[receipts_start..]);
    let first = receipts
        .find("background tasks: none")
        .expect("first queued line");
    let second = receipts
        .find("invalid background exec ID")
        .expect("second queued line");
    let third = receipts
        .find("usage: /tasks [ID | cancel ID]")
        .expect("third queued line");
    assert!(
        first < second && second < third,
        "queued commands reordered: {receipts}"
    );
    drain_until_quiet(&reader, &mut output);
    let draft = output.len();
    input.write_all(b"discard this draft").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, draft, "discard this draft");
    let cleared = output.len();
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, cleared);
    input.write_all(b"verify recovered seat\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"REGRESSION-MARKER-RECOVERED");
    let start = output.len();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let generations = stub.generations();
    assert_eq!(generations.len(), 2);
    assert_eq!(
        count_exact_strings(&generations[1], "verify recovered seat"),
        1
    );
    assert_eq!(
        count_exact_strings(&generations[1], "discard this draft"),
        0
    );
}
