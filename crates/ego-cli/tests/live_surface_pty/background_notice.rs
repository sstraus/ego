use super::*;

// Catches: a producer's 10 KB command is committed as a screenful of composer input.
#[test]
fn background_notice_long_command_is_one_bounded_transcript_row() {
    notice_contract(
        &format!(
            "BG DONE exit=75 log=/tmp/bg.log cmd=python {}",
            "A".repeat(10_240)
        ),
        80,
        false,
    );
}

// Catches: even a short completion is painted after the user prompt instead of as output.
#[test]
fn background_notice_short_command_has_no_composer_prefix() {
    notice_contract("BG DONE exit=0 log=/tmp/bg.log cmd=true", 100, false);
}

// Catches: byte truncation splits a Unicode command or wide characters wrap the notice.
#[test]
fn background_notice_wide_command_respects_terminal_cells() {
    notice_contract(
        &format!("BG DONE exit=1 log=/tmp/bg.log cmd={}", "界".repeat(120)),
        60,
        false,
    );
}

// Catches: the busy admission path commits the raw transport envelope.
#[test]
fn background_notice_busy_submission_is_one_transcript_row() {
    notice_contract("BG DONE exit=0 log=/tmp/bg.log cmd=true", 100, true);
}

// Catches: pasted multiline command text creates additional transcript rows.
#[test]
fn background_notice_multiline_command_shows_only_first_line() {
    notice_contract(
        "BG DONE exit=0 log=/tmp/bg.log cmd=first command\nHIDDEN-SECOND-LINE",
        100,
        false,
    );
}

fn notice_contract(notice: &str, width: u16, busy: bool) {
    let (first, gate) = gated_paced_line("regression_case-RUNNING");
    let mut completions = vec![Reply::Sse(text("regression_case-RECEIVED"))];
    if busy {
        completions.insert(0, first);
    }
    let stub = Stub::start(Plan {
        completions,
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
    ego.file("AGENTS.md", "# background notice fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, width, Some((24, width)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    if busy {
        input.write_all(b"start gated turn\r").unwrap();
        input.flush().unwrap();
        until(&reader, &mut output, b"regression_case-RUNNING");
    }
    let start = output.len();
    // TUIC submits completion notices through the same Ctrl+U/text/CR transport.
    input.write_all(b"\x15").unwrap();
    if notice.contains('\n') {
        input.write_all(b"\x1b[200~").unwrap();
    }
    input.write_all(notice.as_bytes()).unwrap();
    if notice.contains('\n') {
        input.write_all(b"\x1b[201~").unwrap();
    }
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    if busy {
        drain_until_quiet(&reader, &mut output);
        gate.release();
    }
    until_normalized(&reader, &mut output, start, "regression_case-RECEIVED");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let rows = screen_rows(&output, 24, width);
    let capture = rows.join("\n");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    println!("regression_case PTY CAPTURE width={width}:\n{capture}");
    if notice.len() > 10_000 {
        println!("regression_case RAW: {:?}", String::from_utf8_lossy(&output));
    }
    assert_eq!(
        count_exact_strings(&stub.generations()[usize::from(busy)]["messages"], notice),
        1
    );
    let matching: Vec<_> = rows
        .iter()
        .filter(|row| row.contains("background task done"))
        .collect();
    assert_eq!(matching.len(), 1, "one notice row: {capture}");
    assert!(
        unicode_width::UnicodeWidthStr::width(matching[0].trim_end()) < usize::from(width),
        "notice fits in terminal cells: {capture}"
    );
    assert!(
        capture.contains("ego "),
        "notice did not flood prior transcript: {capture}"
    );
    assert!(
        !matching[0].trim_start().starts_with('>'),
        "notice is not composer input: {capture}"
    );
    assert!(
        matching[0].contains("exit "),
        "exit status remains visible: {capture}"
    );
    assert!(
        matching[0].contains("log /tmp/bg.log"),
        "log remains visible: {capture}"
    );
    assert!(
        !capture.contains("BG DONE"),
        "raw notice must not remain: {capture}"
    );
    assert!(
        !capture.contains("HIDDEN-SECOND-LINE"),
        "first command line only: {capture}"
    );
    if notice.len() > usize::from(width) || notice.contains('\n') {
        assert!(
            matching[0].contains('…'),
            "truncation is visible: {capture}"
        );
    }
}
