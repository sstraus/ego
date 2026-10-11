use super::*;

// Catches: while a turn runs the Esc affordance shares the composer row with
// the prompt ("Esc cancels | > [") or the working row wraps into it.
#[test]
fn working_line_is_one_row_above_the_composer_before_the_reply_at_120_columns() {
    working_line_contract(40, 120, false);
}

#[test]
fn working_line_is_one_row_above_the_composer_while_streaming_at_120_columns() {
    working_line_contract(40, 120, true);
}

#[test]
fn working_line_is_one_row_above_the_composer_before_the_reply_at_80_columns() {
    working_line_contract(24, 80, false);
}

#[test]
fn working_line_is_one_row_above_the_composer_while_streaming_at_80_columns() {
    working_line_contract(24, 80, true);
}

fn working_line_contract(rows: u16, cols: u16, streaming: bool) {
    let (reply, gate) = if streaming {
        let (reply, gate) = gated_paced_line("PARTIAL-ANSWER\n");
        (reply, Some(gate))
    } else {
        (paced_line("DELAYED-ANSWER", Duration::from_secs(3)), None)
    };
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# live surface fixture\n");
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    let (input, dsr_responder, writer_join) =
        writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr_responder,
        Some((rows, cols)),
    );
    let mut input = input;
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust flush");
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"working line\r").expect("submit turn");
    input.flush().expect("submit flush");
    until_normalized(
        &reader,
        &mut output,
        0,
        if streaming {
            "PARTIAL-ANSWER"
        } else {
            "Working"
        },
    );
    drain_until_quiet(&reader, &mut output);
    let mut screen = vt100::Parser::new(rows, cols, 0);
    screen.process(&output);
    let visible = screen.screen().contents();
    let lines: Vec<_> = visible.lines().map(str::trim_end).collect();
    let working: Vec<_> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("Working"))
        .collect();
    assert_eq!(working.len(), 1, "one working line expected: {visible:?}");
    let (at, line) = working[0];
    assert!(
        line.contains("Esc to interrupt")
            && line.split_whitespace().any(|word| {
                word.strip_suffix('s')
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            }),
        "working line must carry elapsed seconds and the Esc affordance: {visible:?}"
    );
    assert!(
        !line.contains('>') && !line.contains('['),
        "working line must not hold the prompt or a bracket: {visible:?}"
    );
    assert_eq!(
        lines.get(at + 1).copied(),
        Some(">"),
        "the composer must own the row below the working line: {visible:?}"
    );
    assert!(
        lines
            .iter()
            .skip_while(|row| !row.starts_with("> working line"))
            .all(|row| !row.contains("Esc cancels") && !row.contains("| >") && !row.contains('[')),
        "no row may fold the affordance into the prompt or show a stray bracket: {visible:?}"
    );

    if let Some(gate) = gate {
        gate.release();
    }
    let finish_start = output.len();
    until_prompt_from(&reader, &mut output, finish_start);
    input.write_all(b"/quit\r").expect("quit");
    input.flush().expect("quit flush");
    assert!(bounded_exit(child).success());
    drop(input);
    drop(pair);
    reader_join.join().expect("reader thread");
    writer_join.join().expect("writer thread");
}
