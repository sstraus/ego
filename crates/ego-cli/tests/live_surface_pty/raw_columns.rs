use super::*;

/// `εgo: Resume model changed: anthropic/claude-opus-5-5 -> ollama/qwen3`
/// soft-wraps at this width with the space after `->` in the last cell.
const COLS: u16 = 56;
const ROWS: u16 = 24;
/// Key hints continue the header note, aligned under its text: the width of
/// the `εgo: ` prefix.
const HINT_INDENT: usize = 5;
const MODEL_CHANGE: &str = "Resume model changed: anthropic/claude-opus-5-5 -> ollama/qwen3";

// Catches: raw LF leaves following startup or replay lines at the preceding
// column (the startup drain wrote rows without CRLF while raw mode was on).
#[test]
fn raw_terminal_startup_resume_and_exit_keep_column_zero() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("REGRESSION-MARKER\n\nSECOND-LINE-337"))],
        ..Plan::default()
    });
    let places = support::Places::new();
    let original = places
        .ego(&stub)
        .env("ANTHROPIC_API_KEY", "sk-test")
        .args([
            "--model",
            "anthropic/claude-opus-5-5",
            "--thinking",
            "medium",
            "--sandbox",
            "off",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "first",
        ])
        .run();
    assert_eq!(original.code, Some(0), "{}", original.stderr);
    places.file("AGENTS.md", "# raw column fixture\n");
    // A pre-activation diagnostic that wraps at this width.
    places
        .ego(&stub)
        .home_file("config.toml", "model='ollama/qwen3'\n");
    let lines = drive(places.ego(&stub).subcommand("resume").args([
        support::FIXTURE_SESSION_ID,
        "--sandbox",
        "off",
    ]));
    let shown = lines.join("\n");
    for needle in [
        concat!("ego ", env!("CARGO_PKG_VERSION")),
        "Resume model changed",
        "unsent drafts discarded",
        "To resume: ego resume",
    ] {
        assert_eq!(
            column_of(&lines, needle),
            0,
            "{needle:?} does not start at column 0:\n{shown}"
        );
    }
    assert_hints_aligned(&lines);
    assert_eq!(
        column_of(&lines, "REGRESSION-MARKER"),
        column_of(&lines, "REGRESSION-MARKER"),
        "the second replayed line starts where the first one ended:\n{shown}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains(MODEL_CHANGE) && line.chars().count() > usize::from(COLS)),
        "the wrapped diagnostic lost word spacing or did not wrap:\n{shown}"
    );
}

// Catches: a fresh launch (no replay) prints its header and key hints as a
// staircase after the trust answer.
#[test]
fn raw_terminal_fresh_launch_keeps_column_zero() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# raw column fixture\n");
    let lines =
        drive(
            places
                .ego(&stub)
                .conversing()
                .args(["--model", "ollama/qwen3", "--sandbox", "off"]),
        );
    let shown = lines.join("\n");
    for needle in [
        concat!("ego ", env!("CARGO_PKG_VERSION")),
        "unsent drafts discarded",
    ] {
        assert_eq!(
            column_of(&lines, needle),
            0,
            "{needle:?} does not start at column 0:\n{shown}"
        );
    }
    assert_hints_aligned(&lines);
}

/// Answer trust, wait for the idle composer, end with Ctrl+D and return the
/// whole emulated history as logical lines.
fn drive(ego: Ego) -> Vec<String> {
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("pty");
    let child = pair.slave.spawn_command(ego.pty_command()).expect("spawn");
    drop(pair.slave);
    let (mut input, dsr, writer_join) = writer_owner(pair.master.take_writer().expect("writer"));
    let (reader, reader_join) = stream_with_screen(
        pair.master.try_clone_reader().expect("reader"),
        dsr,
        Some((ROWS, COLS)),
    );
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").expect("trust answer");
    input.flush().expect("trust answer flush");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    input.write_all(b"\x04").expect("interactive EOF");
    input.flush().expect("interactive EOF flush");
    let status = support::wait_pty_child(child, PROGRESS_DEADLINE, &output)
        .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));
    drain_until_quiet(&reader, &mut output);
    drop(input);
    drop(pair.master);
    reader_join.join().expect("PTY reader");
    writer_join.join().expect("PTY writer");
    while let Ok(chunk) = reader.try_recv() {
        output.extend(chunk);
    }
    assert!(status.success(), "{}", normalized(&output));
    logical_lines(&output)
}

/// Every row ever shown, scrollback first, with soft-wrapped rows joined and
/// empty cells kept as spaces: a trimmed row end would hide a lost space.
fn logical_lines(output: &[u8]) -> Vec<String> {
    let mut parser = vt100::Parser::new(ROWS, COLS, 10_000);
    parser.process(output);
    parser.screen_mut().set_scrollback(usize::MAX);
    let depth = parser.screen().scrollback();
    let mut rows = Vec::new();
    for offset in (1..=depth).rev() {
        parser.screen_mut().set_scrollback(offset);
        rows.push(row_cells(parser.screen(), 0));
    }
    parser.screen_mut().set_scrollback(0);
    rows.extend((0..ROWS).map(|row| row_cells(parser.screen(), row)));
    let mut lines = Vec::new();
    let mut current = String::new();
    for (text, wrapped) in rows {
        current.push_str(&text);
        if !wrapped {
            lines.push(current.trim_end().to_owned());
            current.clear();
        }
    }
    lines.push(current.trim_end().to_owned());
    lines
}

fn row_cells(screen: &vt100::Screen, row: u16) -> (String, bool) {
    let text = (0..COLS)
        .filter_map(|col| screen.cell(row, col))
        .filter(|cell| !cell.is_wide_continuation())
        .map(|cell| match cell.contents() {
            "" => " ",
            contents => contents,
        })
        .collect();
    (text, screen.row_wrapped(row))
}

fn assert_hints_aligned(lines: &[String]) {
    for needle in ["Esc interrupts", "Option+P models"] {
        assert_eq!(
            column_of(lines, needle),
            HINT_INDENT,
            "{needle:?} is not aligned under the header:\n{}",
            lines.join("\n")
        );
    }
}

/// Start column of the first logical line containing `needle`; a leading
/// `εgo: ` note prefix counts as part of the row.
fn column_of(lines: &[String], needle: &str) -> usize {
    let line = lines
        .iter()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} never shown:\n{}", lines.join("\n")));
    line.chars().take_while(|ch| *ch == ' ').count()
}
