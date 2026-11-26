use super::*;

// Catches: /setup still enters provider-backed diagnostics instead of a human settings menu.
#[test]
fn slash_setup_opens_settings_without_model_generation_and_esc_keeps_config() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    let original = "model='ollama/qwen3'\nmode='plan'\n";
    std::fs::write(places.home_path().join("config.toml"), original).unwrap();
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--yolo",
        "--trust-workspace",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"/setup\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Choose where to save");
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Setup · Settings");
    // Catches: the immediate-number rule for user questions leaks into
    // paged setup lists and opens a setting before Enter confirms it.
    let selection = output.len();
    input.write_all(b"2").unwrap();
    input.flush().unwrap();
    until_from(&reader, &mut output, selection, b"> 2.");
    drain_until_quiet(&reader, &mut output);
    assert!(
        !normalized(&output[selection..]).contains("effective: yolo"),
        "a setup digit opened the setting before Enter"
    );
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, selection, "effective: yolo");
    let return_start = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, return_start, "Setup · Settings");
    let exit_start = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, exit_start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        std::fs::read_to_string(places.home_path().join("config.toml")).unwrap(),
        original
    );
    assert!(stub.generations().is_empty());
}

fn send(input: &mut PtyWriter, keys: &[u8]) {
    input.write_all(keys).unwrap();
    input.flush().unwrap();
}

// Catches: the real `ego setup` terminal entry saves on a bare Enter at the
// review, or never reaches publication after an explicit Yes.
#[test]
fn ego_setup_review_defaults_to_no_and_saves_only_on_yes() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    let config = places.home_path().join("config.toml");
    std::fs::write(&config, "mode='plan'\n").unwrap();
    let ego = places.ego(&stub).subcommand("setup");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until_normalized(&reader, &mut output, 0, "Choose where to save");
    send(&mut input, b"\r");
    until_normalized(&reader, &mut output, 0, "Setup · Settings");
    // Search is the first row; mode leads the Access and security group (second row); stage `default`.
    send(&mut input, b"\x1b[B\r");
    until_normalized(&reader, &mut output, 0, "Change value");
    send(&mut input, b"\r");
    until_normalized(&reader, &mut output, 0, "Choose mode");
    let mut start = output.len();
    send(&mut input, b"\x1b[B\r");
    for (attempt, answer) in [(0, b"\r".as_slice()), (1, b"\x1b[B\r".as_slice())] {
        if attempt == 0 {
            until_normalized(&reader, &mut output, start, "mode = \"default\"");
        } else {
            // The notice is drawn above the list it precedes.
            drain_until_quiet(&reader, &mut output);
        }
        start = output.len();
        // End selects "Discard and exit"; one Up selects "Review and save".
        send(&mut input, b"\x1b[F\x1b[A\r");
        until_normalized(&reader, &mut output, start, "Save these changes?");
        start = output.len();
        send(&mut input, answer);
        if attempt == 0 {
            until_normalized(
                &reader,
                &mut output,
                start,
                "Not saved. Your edits are still staged.",
            );
            assert_eq!(std::fs::read_to_string(&config).unwrap(), "mode='plan'\n");
        }
    }
    let status = session.quit(input);
    assert!(status.success(), "{}", String::from_utf8_lossy(&output));
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(
        saved.contains("default") && !saved.contains("plan"),
        "{saved}"
    );
    assert!(stub.generations().is_empty());
}

// Catches: `/setup` confirms a review the terminal cannot show in full, or
// treats a blank answer as consent.
#[test]
fn slash_setup_prints_the_whole_review_and_saves_only_on_yes() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    let config = places.home_path().join("config.toml");
    let original = "model='ollama/qwen3'\nmode='plan'\n";
    std::fs::write(&config, original).unwrap();
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--yolo",
        "--trust-workspace",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 40, 100, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    send(&mut input, b"/setup\r");
    until_normalized(&reader, &mut output, start, "Choose where to save");
    send(&mut input, b"\r");
    until_normalized(&reader, &mut output, start, "Setup · Settings");
    // Search is the first row; mode leads the Access and security group (second row).
    send(&mut input, b"\x1b[B\r");
    until_normalized(&reader, &mut output, start, "Change value");
    send(&mut input, b"\r");
    until_normalized(&reader, &mut output, start, "Choose mode");
    for (attempt, answer) in [(0, b"\r".as_slice()), (1, b"y\r".as_slice())] {
        let round = output.len();
        if attempt == 0 {
            send(&mut input, b"\x1b[B\r");
        }
        until_normalized(&reader, &mut output, round, "User defaults · 1–10 of ");
        // Catches: page keys are swallowed by the session selector, or save
        // remains reachable only on the last page.
        send(&mut input, b"\x1b[6~");
        until_normalized(&reader, &mut output, round, "11–20 of ");
        send(&mut input, b"\x1b[5~");
        until_normalized(&reader, &mut output, round, "1–10 of ");
        send(&mut input, b"\x1b[F");
        drain_until_quiet(&reader, &mut output);
        assert!(
            screen_rows(&output, 40, 100)
                .join("\n")
                .contains("Discard and exit")
        );
        send(&mut input, b"\x1b[H\x1b[6~s");
        until_normalized(&reader, &mut output, round, "Save these changes? [y/N]");
        let shown = normalized(&output[round..]);
        assert!(shown.contains("mode: \"plan\" -> \"default\""), "{shown}");
        assert!(shown.contains("This session keeps its captured"), "{shown}");
        send(&mut input, answer);
        if attempt == 0 {
            until_normalized(
                &reader,
                &mut output,
                round,
                "Not saved. Your edits are still staged.",
            );
            assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
        } else {
            until_normalized(&reader, &mut output, round, "Saved:");
        }
    }
    until_prompt_from(&reader, &mut output, start);
    send(&mut input, b"/quit\r");
    assert!(session.quit(input).success());
    let saved = std::fs::read_to_string(&config).unwrap();
    assert!(
        saved.contains("default") && !saved.contains("plan"),
        "{saved}"
    );
    assert!(stub.generations().is_empty());
}

fn setup_pty(
    rows: u16,
    config: &str,
) -> (
    PtySession,
    PtyWriter,
    std::sync::mpsc::Receiver<Vec<u8>>,
    Vec<u8>,
    std::path::PathBuf,
    Stub,
    support::Places,
) {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    let path = places.home_path().join("config.toml");
    std::fs::write(&path, config).unwrap();
    let ego = places.ego(&stub).subcommand("setup");
    let (session, mut input, reader) = PtySession::start(&ego, rows, 100, None);
    let mut output = Vec::new();
    until_normalized(&reader, &mut output, 0, "Choose where to save");
    send(&mut input, b"\r");
    until_normalized(&reader, &mut output, 0, "Setup · Settings");
    drain_until_quiet(&reader, &mut output);
    (session, input, reader, output, path, stub, places)
}

// Catches: a view that fills the terminal height ends its last row with a
// newline, so the terminal scrolls and the first title row (the heading of
// the settings list) is never on screen.
#[test]
fn a_full_height_setup_view_keeps_its_first_row_on_screen() {
    let (session, mut input, _reader, output, _path, _stub, _places) =
        setup_pty(24, "mode='plan'\n");
    let rows = screen_rows(&output, 24, 100);
    assert!(
        rows.first()
            .is_some_and(|row| row.contains("Setup · Settings")),
        "first screen row lost to a scroll: {rows:?}"
    );
    send(&mut input, b"\x1b");
    assert!(session.quit(input).success());
}

// Catches: a notice printed between two redraws is erased by the next
// full-screen clear, so "No changes to save." never reaches the operator.
#[test]
fn a_setup_notice_survives_the_next_redraw() {
    let (session, mut input, reader, mut output, _path, _stub, _places) =
        setup_pty(24, "mode='plan'\n");
    let start = output.len();
    // End selects "Discard and exit"; one Up selects "Review and save".
    send(&mut input, b"\x1b[F\x1b[A\r");
    until_normalized(&reader, &mut output, start, "No changes to save.");
    until_normalized(&reader, &mut output, start, "Setup · Settings");
    drain_until_quiet(&reader, &mut output);
    let screen = screen_rows(&output, 24, 100).join("\n");
    assert!(
        screen.contains("No changes to save."),
        "notice erased: {screen}"
    );
    send(&mut input, b"\x1b");
    assert!(session.quit(input).success());
}

// Catches: a long review scrolls its top row off the terminal on every page,
// so the page-top lines (here the mode change itself) are never on screen
// when the operator answers Yes.
#[test]
fn every_review_line_is_on_screen_on_some_page() {
    let (session, mut input, reader, mut output, config, _stub, _places) =
        setup_pty(8, "mode='plan'\n");
    // Search is the first row; mode leads the Access and security group (second row).
    send(&mut input, b"\x1b[B\r");
    until_normalized(&reader, &mut output, 0, "Change value");
    send(&mut input, b"\r");
    until_normalized(&reader, &mut output, 0, "Choose mode");
    drain_until_quiet(&reader, &mut output);
    let mut start = output.len();
    send(&mut input, b"\x1b[B\r");
    until_normalized(&reader, &mut output, start, "mode = \"default\"");
    drain_until_quiet(&reader, &mut output);
    start = output.len();
    send(&mut input, b"\x1b[F\x1b[A\r");
    until_normalized(&reader, &mut output, start, "scroll text");
    drain_until_quiet(&reader, &mut output);
    let mut seen = String::new();
    for _ in 0..12 {
        seen.push_str(&screen_rows(&output, 8, 100).join("\n"));
        seen.push('\n');
        let at = output.len();
        send(&mut input, b"\x1b[C");
        let _ = at;
        drain_until_quiet(&reader, &mut output);
    }
    for needle in [
        "Review:",
        "mode: \"plan\" -> \"default\"",
        "Security-sensitive",
        "Save these changes?",
    ] {
        assert!(seen.contains(needle), "never on screen: {needle}\n{seen}");
    }
    send(&mut input, b"\x1b");
    drain_until_quiet(&reader, &mut output);
    // "Discard and exit", then Yes to discard the staged edit.
    send(&mut input, b"\x1b[F\r");
    drain_until_quiet(&reader, &mut output);
    send(&mut input, b"\x1b[B\r");
    drain_until_quiet(&reader, &mut output);
    assert!(session.quit(input).success());
    assert_eq!(std::fs::read_to_string(config).unwrap(), "mode='plan'\n");
}

// Catches: raw mode swallows Ctrl-C (ISIG is off), so the operator has no
// visible way out of `ego setup` except the Esc key the key line names.
#[test]
fn ctrl_c_leaves_ego_setup_without_saving() {
    let (session, mut input, _reader, _output, config, _stub, _places) =
        setup_pty(24, "mode='plan'\n");
    send(&mut input, b"\x03");
    // A quiet terminal does not establish process termination. Require the
    // Ctrl-C itself to exit within the existing child deadline, with no Esc rescue.
    assert!(session.quit(input).success());
    assert_eq!(std::fs::read_to_string(config).unwrap(), "mode='plan'\n");
}

// Catches: native setup swallows page keys or requires reaching the last row to save.
#[test]
fn setup_page_keys_save_from_a_middle_page() {
    let (session, mut input, reader, mut output, config, _stub, _places) =
        setup_pty(24, "mode='plan'\n");
    send(&mut input, b"\x1b[B\r");
    until_normalized(&reader, &mut output, 0, "Change value");
    send(&mut input, b"\r");
    until_normalized(&reader, &mut output, 0, "Choose mode");
    let start = output.len();
    send(&mut input, b"\x1b[F\x1b[H\x1b[B\r");
    until_normalized(&reader, &mut output, start, "mode = \"default\"");
    drain_until_quiet(&reader, &mut output);
    let start = output.len();
    send(&mut input, b"\x1b[6~");
    drain_until_quiet(&reader, &mut output);
    let frame = screen_rows(&output, 24, 100).join("\n");
    assert!(frame.contains("PgUp/PgDn page"), "{frame}");
    assert!(frame.contains("Home/End first/last"), "{frame}");
    assert!(frame.contains("S review/save"), "{frame}");
    assert!(!frame.contains("> Search settings"), "{frame}");
    assert!(!frame.contains("> Discard and exit"), "{frame}");
    send(&mut input, b"\x1b[5~");
    drain_until_quiet(&reader, &mut output);
    assert!(
        screen_rows(&output, 24, 100)
            .join("\n")
            .contains("> Search settings")
    );
    send(&mut input, b"\x1b[6~s");
    until_normalized(&reader, &mut output, start, "Save these changes?");
    send(&mut input, b"\x1b[B\r");
    assert!(session.quit(input).success());
    let saved = std::fs::read_to_string(config).unwrap();
    assert!(
        saved.contains("default") && !saved.contains("plan"),
        "{saved}"
    );
}
