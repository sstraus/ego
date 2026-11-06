use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

// Catches: permanent startup CPR silence retries forever without a composer.
#[test]
fn permanent_startup_cursor_silence_still_accepts_input() {
    cursor_silence(false);
}

// Catches: clearing the screen after a completed turn re-enters cursor discovery
// and permanently hangs a previously usable terminal when the host stops replying.
#[test]
fn permanent_cursor_silence_after_a_completed_turn_still_accepts_input() {
    cursor_silence(true);
}

fn cursor_silence(after_turn: bool) {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("CURSOR-TURN-COMPLETED"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# Permanent cursor silence fixture\n");
    let replies = Arc::new(AtomicBool::new(after_turn));
    let (mut session, mut input, reader) = PtySession::start_command_with_cursor_replies(
        ego.pty_command(),
        24,
        120,
        Some((24, 120)),
        Some(Arc::clone(&replies)),
    );
    struct Cleanup(Option<Box<dyn portable_pty::ChildKiller + Send + Sync>>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Some(killer) = &mut self.0 {
                let _ = killer.kill();
            }
        }
    }
    let mut cleanup = Cleanup(Some(session.child.clone_killer()));
    let mut output = Vec::new();
    let start = if after_turn {
        until_prompt_from(&reader, &mut output, 0);
        input.write_all(b"complete one turn\r").unwrap();
        input.flush().unwrap();
        until_prompt_after_reply(&reader, &mut output, b"CURSOR-TURN-COMPLETED");
        drain_until_quiet(&reader, &mut output);
        replies.store(false, Ordering::SeqCst);
        let start = output.len();
        // Ctrl+L discards the old screen and genuinely rediscovers the cursor.
        input.write_all(b"\x0c").unwrap();
        input.flush().unwrap();
        start
    } else {
        0
    };
    until_from(&reader, &mut output, start, DSR_QUERY);
    until_prompt_from(&reader, &mut output, start);
    assert_eq!(
        output[start..]
            .windows(DSR_QUERY.len())
            .filter(|bytes| *bytes == DSR_QUERY)
            .count(),
        1,
        "one discovery attempt must suffice even without a reply"
    );
    assert!(session.child.try_wait().unwrap().is_none());
    let help = output.len();
    input.write_all(b"/help\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        help,
        "Unknown slash input is refused locally",
    );
    until_prompt_from(&reader, &mut output, help);
    let draft = "CURSOR-UNSENT-DRAFT";
    input.write_all(draft.as_bytes()).unwrap();
    input.flush().unwrap();
    let mut terminal = vt100::Parser::new(24, 120, 100);
    terminal.process(&output);
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !terminal.screen().contents().contains(draft) {
        let chunk = reader
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|error| {
                panic!(
                    "draft did not appear: {error}; {}",
                    terminal.screen().contents()
                )
            });
        terminal.process(&chunk);
        output.extend(chunk);
    }
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    cleanup.0 = None;
    assert_eq!(stub.generations().len(), usize::from(after_turn));
}
