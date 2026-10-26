use super::*;

// Catches: slash setup bypasses its terminal confirmation or cannot publish a host-approved candidate.
#[test]
fn setup_slash_real_terminal_apply_preserves_captured_session_authority() {
    setup_slash_case(b" \r", Some(b"yes\r"), true);
}

// Catches: cancelling slash setup publishes the prepared candidate.
#[test]
fn setup_slash_real_terminal_cancel_keeps_config_bytes() {
    setup_slash_case(b" \r", Some(b"\x1b"), false);
}

// Catches: cancelling the host item picker still opens publication confirmation.
#[test]
fn setup_slash_item_picker_cancel_keeps_config_bytes() {
    setup_slash_case(b"\x1b", None, false);
}

fn setup_slash_case(pick: &[u8], answer: Option<&[u8]>, applied: bool) {
    let needed = tempfile::tempdir().unwrap();
    let target = needed.path().join("task.md");
    std::fs::write(&target, "outside task").unwrap();
    let omitted = tempfile::tempdir().unwrap();
    let omitted_target = omitted.path().join("omitted.md");
    std::fs::write(&omitted_target, "not selected").unwrap();
    let selection = serde_json::json!([
        {"kind":"read", "path":target, "reason":"task"},
        {"kind":"read", "path":omitted_target, "reason":"not selected"}
    ])
    .to_string();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text(&selection))],
        ..Plan::default()
    });
    let places = support::Places::new();
    places.file("AGENTS.md", "# setup fixture\n");
    let original = "model='ollama/qwen3'\nsandbox='off'\nroots=[]\n";
    std::fs::write(places.home_path().join("config.toml"), original).unwrap();
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--yolo",
        "--trust-workspace",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 32, 140, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"/setup inspect task\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Select setup proposal items");
    input.write_all(pick).unwrap();
    input.flush().unwrap();
    if answer.is_some() {
        until_normalized(&reader, &mut output, start, "Apply? [y/N]");
    }
    assert_eq!(
        std::fs::read_to_string(places.home_path().join("config.toml")).unwrap(),
        original
    );
    let start = if answer.is_some() {
        output.len()
    } else {
        start
    };
    if let Some(answer) = answer {
        input.write_all(answer).unwrap();
        input.flush().unwrap();
    }
    until_normalized(
        &reader,
        &mut output,
        start,
        if applied {
            "Setup publication"
        } else {
            "Setup cancelled"
        },
    );
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let current = std::fs::read_to_string(places.home_path().join("config.toml")).unwrap();
    if applied {
        assert!(
            current.contains(&needed.path().display().to_string()),
            "{current}"
        );
    } else {
        assert_eq!(current, original);
    }
    assert!(
        !current.contains(&omitted.path().display().to_string()),
        "unselected item persisted: {current}"
    );
    assert_eq!(
        std::fs::read_to_string(omitted_target).unwrap(),
        "not selected"
    );
    assert_eq!(std::fs::read_to_string(target).unwrap(), "outside task");
    assert_eq!(
        stub.generations().len(),
        1,
        "slash setup must not start ordinary generation"
    );
}

// Catches: raw terminal Ctrl-C queues an idle interrupt instead of stopping setup inference.
#[test]
fn setup_slash_raw_ctrl_c_stops_stalled_inference_without_publication() {
    let gate = support::ReleaseGate::new();
    let stub = Stub::start(Plan {
        completions: vec![Reply::GatedPaced {
            head: String::new(),
            tail: text("[]"),
            gate: gate.clone(),
        }],
        ..Plan::default()
    });
    let places = support::Places::new();
    let original = "model='ollama/qwen3'\nsandbox='off'\nroots=[]\n";
    std::fs::write(places.home_path().join("config.toml"), original).unwrap();
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--yolo",
        "--trust-workspace",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 32, 140, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"/setup inspect task\r").unwrap();
    input.flush().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while stub.generations().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "setup inference did not start"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Bound the scripted server even when the regression assertion fails.
    let release = gate.clone();
    let release_thread = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(5));
        release.release();
    });
    let start = output.len();
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Setup cancelled");
    gate.release();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    release_thread.join().unwrap();
    assert_eq!(
        std::fs::read_to_string(places.home_path().join("config.toml")).unwrap(),
        original
    );
    assert_eq!(stub.generations().len(), 1);
}

/// Catches: setup calls a flag-selected active model unconfigured without naming its separate authority.
#[test]
fn interactive_setup_explains_session_model_vs_interpreter_model() {
    let mut captures = Vec::new();
    for (rows, columns) in [(24, 80), (40, 120)] {
        let stub = Stub::start(Plan::default());
        let places = support::Places::new();
        let original = "sandbox='off'\nroots=[]\n";
        std::fs::write(places.home_path().join("config.toml"), original).unwrap();
        let ego = places
            .ego(&stub)
            .conversing()
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--yolo",
                "--trust-workspace",
            ])
            .env("TERM", "xterm-256color");
        let (session, mut input, reader) = PtySession::start(&ego, rows, columns, None);
        let mut output = Vec::new();
        until_prompt_from(&reader, &mut output, 0);
        let start = output.len();
        input.write_all(b"/setup inspect task\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "Select setup proposal items");
        input.write_all(b"\x1b").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "Setup cancelled");
        until_prompt_from(&reader, &mut output, start);
        drain_until_quiet(&reader, &mut output);
        if let Ok(root) = std::env::var("EGO_UX_CAPTURE_DIR") {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(format!("{root}/{columns}x{rows}.ansi"), &output).unwrap();
        }
        captures.push(normalized(&output[start..]));
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        assert_eq!(
            std::fs::read_to_string(places.home_path().join("config.toml")).unwrap(),
            original
        );
        assert!(
            stub.generations().is_empty(),
            "no configured interpreter must not generate"
        );
    }
    for capture in captures {
        assert!(capture.contains("Session model: ollama/qwen3"), "{capture}");
        assert!(
            capture.contains("no persistent setup interpreter model"),
            "{capture}"
        );
        assert!(capture.contains("deterministic inventory"), "{capture}");
    }
}

/// Catches: setup repeats finding IDs beside row numbers and overflows with absolute home paths.
#[test]
fn followup_setup_rows_have_one_number_and_bounded_home_paths() {
    let mut captures = Vec::new();
    for (rows, columns) in [(24, 80), (40, 120)] {
        let stub = Stub::start(Plan::default());
        let places = support::Places::new();
        std::fs::write(
            places.home_path().join("config.toml"),
            "sandbox='off'\nroots=[]\n",
        )
        .unwrap();
        let ego = places
            .ego(&stub)
            .conversing()
            .args([
                "--model",
                "ollama/qwen3",
                "--sandbox",
                "off",
                "--yolo",
                "--trust-workspace",
            ])
            .env("TERM", "xterm-256color");
        let (session, mut input, reader) = PtySession::start(&ego, rows, columns, None);
        let mut output = Vec::new();
        until_prompt_from(&reader, &mut output, 0);
        let start = output.len();
        input.write_all(b"/setup inspect task\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "Up/Down moves");
        drain_until_quiet(&reader, &mut output);
        captures.push((
            columns,
            normalized(&output[start..]),
            places.home_path().display().to_string(),
        ));
        input.write_all(b"\x1b").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "Setup cancelled");
        until_prompt_from(&reader, &mut output, start);
        input.write_all(b"/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        assert_eq!(
            std::fs::read_to_string(places.home_path().join("config.toml")).unwrap(),
            "sandbox='off'\nroots=[]\n"
        );
        assert!(stub.generations().is_empty());
    }
    for (columns, capture, home) in captures {
        let rows = capture
            .lines()
            .filter(|line| line.contains("[ ]"))
            .collect::<Vec<_>>();
        assert!(!rows.is_empty(), "{capture}");
        for row in rows {
            let label = row.split_once(". ").expect("row number").1;
            assert!(
                !label.chars().next().unwrap().is_ascii_digit(),
                "duplicate ID: {row}"
            );
            assert!(!label.contains(&home), "absolute home path: {row}");
            assert!(
                row.trim_start_matches('\r').chars().count() < usize::from(columns),
                "row overflows terminal: {row}"
            );
        }
    }
}

/// Catches: a wrapped setup receipt leaves the old picker heading suffix on screen.
#[test]
fn setup_picker_heading_rows_have_no_external_notice_overdraw() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    std::fs::write(
        places.home_path().join("config.toml"),
        "sandbox='off'\nroots=[]\n",
    )
    .unwrap();
    let ego = places
        .ego(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--yolo",
            "--trust-workspace",
        ])
        .env("TERM", "xterm-256color");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 80, Some((24, 80)));
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"/setup inspect task\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Up/Down moves");
    until_normalized(&reader, &mut output, start, "not this session selection.");
    drain_until_quiet(&reader, &mut output);
    let mut terminal = vt100::Parser::new(24, 80, 0);
    terminal.process(&output);
    // Read physical cells: Screen::contents joins soft-wrapped receipt rows,
    // which would hide the exact row footprint this regression protects.
    let visible = (0..24)
        .map(|row| {
            (0..80)
                .filter_map(|column| terminal.screen().cell(row, column))
                .filter(|cell| !cell.is_wide_continuation())
                .map(|cell| match cell.contents() {
                    "" => " ",
                    text => text,
                })
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>();
    if let Ok(root) = std::env::var("EGO_UX_CAPTURE_DIR") {
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(format!("{root}/80x24.ansi"), &output).unwrap();
        std::fs::write(format!("{root}/80x24.txt"), visible.join("\n")).unwrap();
    }
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Setup cancelled");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let first_option = visible
        .iter()
        .position(|row| row.contains("[ ] 1. "))
        .expect("first setup item");
    assert!(first_option >= 3, "heading footprint missing: {visible:?}");
    assert_eq!(
        visible[first_option - 3..first_option]
            .iter()
            .map(|row| row.trim())
            .collect::<Vec<_>>(),
        [
            "configuration, not this session selection.",
            "Select setup proposal items; only selected items enter the preview.",
            "Final confirmation still required; Esc cancels without changes.",
        ],
        "every visible heading row must contain only its intended text: {visible:?}",
    );
    assert_eq!(
        std::fs::read_to_string(places.home_path().join("config.toml")).unwrap(),
        "sandbox='off'\nroots=[]\n"
    );
    assert!(stub.generations().is_empty());
}
