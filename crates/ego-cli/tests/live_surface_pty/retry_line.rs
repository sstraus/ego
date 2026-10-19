use super::*;

// Catches: raw provider errors and retry notes accumulate instead of replacing
// the live row, or recovery drops the cause instead of settling one receipt.
#[test]
fn three_provider_retries_replace_one_row_and_recovery_keeps_one_receipt() {
    retry_contract("xterm-256color", false, true);
}

// Catches: explicit plain output still replaces retry diagnostics.
#[test]
fn plain_retry_notices_are_appended() {
    retry_contract("xterm-256color", true, false);
}

// Catches: a dumb terminal receives live retry repaint state.
#[test]
fn dumb_retry_notices_are_appended() {
    retry_contract("dumb", false, false);
}

fn retry_contract(term: &str, plain: bool, live: bool) {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Status(503),
            Reply::Status(503),
            Reply::StatusRetryAfter {
                status: 503,
                seconds: 3,
            },
            Reply::Sse(text("RETRY-RECOVERED")),
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--retry",
            "3",
        ])
        .env("TERM", term);
    let ego = if plain { ego.arg("--plain") } else { ego };
    ego.file("AGENTS.md", "# retry surface fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 180, Some((40, 180)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"retry line\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "attempt 3/3");
    drain_until_quiet(&reader, &mut output);
    let before = screen_rows(&output, 40, 180).join("\n");
    if !live {
        assert!(
            cause_colours(&output)
                .iter()
                .all(|colour| *colour == vt100::Color::Default)
        );
    }
    let live_count = before.matches("503").count();
    let start = output.len();
    until_normalized(&reader, &mut output, start, "RETRY-RECOVERED");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let after = screen_rows(&output, 40, 180).join("\n");
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    eprintln!("BEFORE SCREEN:\n{before}\nAFTER SCREEN:\n{after}");
    if !live {
        assert!(live_count >= 3, "plain notices must append: {before}");
        return;
    }
    assert_eq!(live_count, 1, "one live cause row: {before}");
    assert_eq!(
        after.matches("503").count(),
        1,
        "one settled cause: {after}"
    );
    assert!(!after.contains("retrying in"), "stale retry row: {after}");
}

// Catches: retry and recovery both use the neutral diagnostic style.
#[test]
fn overload_retry_is_red_and_recovery_receipt_is_green() {
    retry_colour_contract(true, false);
}

// Catches: settling an exhausted retry applies success colour unconditionally.
#[test]
fn exhausted_retry_receipt_stays_red_without_green() {
    retry_colour_contract(false, false);
}

// Catches: live retry painting bypasses NO_COLOR on an interactive terminal.
#[test]
fn no_color_retry_and_recovery_remain_uncoloured() {
    retry_colour_contract(true, true);
}

fn retry_colour_contract(recovers: bool, no_color: bool) {
    let stub = Stub::start(Plan {
        completions: vec![
            Reply::Status(503),
            Reply::StatusRetryAfter {
                status: 503,
                seconds: 3,
            },
            if recovers {
                Reply::Sse(text("RETRY-RECOVERED"))
            } else {
                Reply::Status(503)
            },
        ],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--retry",
            "2",
        ])
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor");
    let ego = if no_color {
        ego.env("NO_COLOR", "1")
    } else {
        ego
    };
    ego.file("AGENTS.md", "# retry colour fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 180, Some((40, 180)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    input.write_all(b"retry colours\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "attempt 2/2");
    drain_until_quiet(&reader, &mut output);
    let before = output.clone();
    let start = output.len();
    if recovers {
        until_normalized(&reader, &mut output, start, "RETRY-RECOVERED");
    }
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let after = output.clone();
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let red = vt100::Color::Rgb(255, 107, 128); // Palette::ERROR
    let green = vt100::Color::Rgb(80, 200, 80); // Palette::ADDED_FG (successful tool rows)
    let expected_live = if no_color { vt100::Color::Default } else { red };
    let expected_receipt = if no_color {
        vt100::Color::Default
    } else if recovers {
        green
    } else {
        red
    };
    let live = cause_colours(&before);
    let settled = cause_colours(&after);
    assert_eq!(live, vec![expected_live], "one red live cause row");
    assert_eq!(
        settled,
        vec![expected_receipt],
        "one outcome-coloured receipt"
    );
    if !recovers {
        assert!(
            !String::from_utf8_lossy(&after).contains("\x1b[38;2;80;200;80m"),
            "failed turn emitted success colour"
        );
    }
}

fn cause_colours(output: &[u8]) -> Vec<vt100::Color> {
    let mut parser = vt100::Parser::new(40, 180, 0);
    parser.process(output);
    let screen = parser.screen();
    let mut colours = Vec::new();
    for row in 0..40 {
        for col in 0..178 {
            let text: String = (col..col + 3)
                .map(|column| screen.cell(row, column).unwrap().contents())
                .collect();
            if text == "503" {
                colours.push(screen.cell(row, col).unwrap().fgcolor());
            }
        }
    }
    colours
}
