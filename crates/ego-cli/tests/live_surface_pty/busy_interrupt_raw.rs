use super::*;
use pty_termios::sys::termios::LocalFlags;

// Catches: Ctrl+C during a turn ends the editor read like a finished line, so
// the tty is cooked while the owner waits for the turn to accept the cancel.
// Keys typed in that window are echoed by the tty and Enter reaches the editor
// as Ctrl+J, a newline that never submits (/expand stayed a draft under load).
#[test]
fn busy_ctrl_c_keeps_the_terminal_raw_while_the_turn_cancels() {
    let (first, gate) = gated_paced_line("RAW-879-STREAMING");
    let stub = Stub::start(Plan {
        completions: vec![first],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--trust-workspace",
    ]);
    ego.file("AGENTS.md", "# busy interrupt fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    input.write_all(b"stream output\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, 0, "RAW-879-STREAMING");
    // The slave's line discipline, read through the master: ICANON is cooked.
    let cooked = || {
        session
            .pair
            .master
            .get_termios()
            .expect("pty termios")
            .local_flags
            .contains(LocalFlags::ICANON)
    };
    let raw_while_busy = !cooked();

    let start = output.len();
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    // Sample from the key until the cancel is visible and a little after, so
    // the whole window between the read's end and the next read is covered.
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    let mut settled: Option<Instant> = None;
    let mut cooked_samples = 0_usize;
    while settled.is_none_or(|at| at.elapsed() < Duration::from_millis(500))
        && Instant::now() < deadline
    {
        if cooked() {
            cooked_samples += 1;
        }
        let mut arrived = false;
        while let Ok(chunk) = reader.try_recv() {
            output.extend(chunk);
            arrived = true;
        }
        if arrived
            && settled.is_none()
            && normalized(&output[start..]).contains("cancelling active work")
        {
            settled = Some(Instant::now());
        }
    }
    let cancelled = settled.is_some();

    gate.release();
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    let status = session.quit(input);
    assert!(
        raw_while_busy,
        "the composer owns a raw tty during the turn"
    );
    assert!(
        cancelled,
        "Ctrl+C never reported the cancel: {:?}",
        normalized(&output[start..])
    );
    assert_eq!(
        cooked_samples, 0,
        "the tty left raw mode after Ctrl+C during a turn"
    );
    // The session exit carries its last turn, which this Ctrl+C interrupted.
    assert_eq!(status.exit_code(), 130, "REPL exit: {status:?}");
}
