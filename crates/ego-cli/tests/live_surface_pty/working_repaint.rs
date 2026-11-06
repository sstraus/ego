use super::*;

// Catches: a wrapped submission moves the live origin and strands its first frame.
#[test]
fn working_repaint_after_wrapped_submission_has_one_live_row() {
    repaint_contract("wrapped");
}

// Catches: an immediate notice preserves the first Working frame above the next one.
#[test]
fn working_repaint_after_first_second_notice_has_one_live_row() {
    repaint_contract("notice");
}

// Catches: first-second resize reconstructs the origin from a different row count.
#[test]
fn working_repaint_after_first_second_resize_has_one_live_row() {
    repaint_contract("resize");
}

// Catches: a host-only suspension at the bottom allocates a second live region.
#[test]
fn working_repaint_after_host_suspension_has_one_live_row() {
    repaint_contract("host");
}

// Catches: completion before the first timer tick commits the initial Working frame.
#[test]
fn working_repaint_fast_reply_retires_the_live_row() {
    repaint_contract("fast");
}

fn repaint_contract(case: &str) {
    let (reply, gate) = gated_paced_line("REPAINT-HELD\n");
    let stub = Stub::start(Plan {
        completions: vec![reply],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# working repaint fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 8, 80, Some((8, 80)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, 0);
    let prompt = if case == "wrapped" {
        "wrapped prompt ".repeat(12)
    } else {
        "working repaint".to_owned()
    };
    input.write_all(b"\x1b[200~").unwrap();
    input.write_all(prompt.as_bytes()).unwrap();
    input.write_all(b"\x1b[201~\r").unwrap();
    input.flush().unwrap();
    until(&reader, &mut output, b"REPAINT-HELD");
    drain_until_quiet(&reader, &mut output);
    let transition = output.len();
    let mut parser = vt100::Parser::new(8, 80, 100);
    parser.process(&output);
    match case {
        "notice" => {
            input.write_all(b"/mcp\r").unwrap();
            input.flush().unwrap();
            until_normalized(&reader, &mut output, transition, "mcp:");
        }
        "host" => {
            input.write_all(b"\x1b[Z").unwrap();
            input.flush().unwrap();
        }
        "resize" => {
            session
                .pair
                .master
                .resize(PtySize {
                    rows: 8,
                    cols: 60,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            parser.screen_mut().set_size(8, 60);
        }
        _ => {}
    }
    let visible = if case == "fast" {
        gate.release();
        until_idle_status_ready(&reader, &mut output, transition);
        drain_until_quiet(&reader, &mut output);
        parser.process(&output[transition..]);
        parser.screen().contents()
    } else {
        until_normalized(&reader, &mut output, transition, "Working 2s");
        drain_until_quiet(&reader, &mut output);
        parser.process(&output[transition..]);
        let visible = parser.screen().contents();
        let end = output.len();
        gate.release();
        until_idle_status_ready(&reader, &mut output, end);
        visible
    };
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        visible.matches("Working").count(),
        usize::from(case != "fast"),
        "{case}: stale Working frame in final grid: {visible:?}"
    );
    assert_eq!(
        visible.lines().filter(|row| row.trim() == ">").count(),
        1,
        "{case}: one live composer expected: {visible:?}"
    );
    if case == "host" {
        assert!(
            visible.contains("local command pending until the current turn ends"),
            "the shared owner must retain the receipt: {visible:?}"
        );
    }
}

// Catches: growing the PTY strands an old footer above the new live frame.
#[test]
fn height_changes_keep_one_live_footer() {
    let (reply, gate) = gated_paced_line("HEIGHT-HELD\n");
    let mut session = modal_session_with(vec![reply, Reply::Sse(text("HEIGHT-SECOND-DONE"))]);
    session.press(b"/help\r");
    session.wait("Retry a waiting provider now");
    let mut failures = Vec::new();
    for (phase, rows) in [("idle shrink", 24), ("idle grow", 40)] {
        session.resize(rows, 120);
        session.wait("qwen3");
        let grid = session.screen().join("\n");
        println!("HEIGHT {phase}: {grid:?}");
        if grid.matches("qwen3 · ").count() != 1 {
            failures.push(format!("{phase}: {grid}"));
        }
    }
    session.press(b"height first\r");
    session.wait("HEIGHT-HELD");
    for (phase, rows) in [("working shrink", 24), ("working grow", 40)] {
        session.resize(rows, 120);
        session.wait("Working");
        let grid = session.screen().join("\n");
        println!("HEIGHT {phase}: {grid:?}");
        if grid.matches("Working").count() != 1 {
            failures.push(format!("{phase}: {grid}"));
        }
    }
    session.press(b"height second\r");
    gate.release();
    session.wait("HEIGHT-SECOND-DONE");
    let grid = session.screen().join("\n");
    println!("HEIGHT completed queued turn: {grid:?}");
    session.finish();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
