use super::*;

// Catches: a resize that lands right after a host-accepted line records the
// new prompt's cursor row as a frame cursor and moves the origin up by the
// status row, so each repaint erases one more receipt row.
#[test]
fn resize_right_after_accept_keeps_every_receipt_row() {
    let (reply, gate) = gated_paced_line("ACCEPT-HELD\n");
    let mut session = modal_session_with(vec![reply, Reply::Sse(text("ACCEPT-DONE"))]);
    session.press(b"accept first\r");
    session.wait("ACCEPT-HELD");
    let mut failures = Vec::new();
    for round in 0..6 {
        let line = format!("queued receipt {round}");
        drain_until_quiet(&session.reader, &mut session.output);
        session.mark = session.output.len();
        session
            .input
            .write_all(format!("{line}\r").as_bytes())
            .expect("keys");
        session.input.flush().expect("keys flush");
        // No settle between the Enter and the resize: the resize must reach
        // the editor before it paints the post-accept prompt.
        let rows = if round % 2 == 0 { 30 } else { 40 };
        session.resize_now(rows, 120);
        session.wait("Working");
        let grid = session.screen().join("\n");
        println!("ACCEPT round {round}: {grid:?}");
        if !grid.contains("> accept first") {
            failures.push(format!("round {round}: first receipt row lost: {grid}"));
        }
        if grid.matches("Working").count() != 1 {
            failures.push(format!("round {round}: not one live frame: {grid}"));
        }
    }
    gate.release();
    session.wait("ACCEPT-DONE");
    session.finish();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
