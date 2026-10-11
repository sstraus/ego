use super::*;

const SEQ_FOUR: (&str, &str) = ("exec", r#"{"argv":["seq","4","6"],"timeout_ms":5000}"#);

/// A first turn that fills the screen, so the next modal sits flush at the
/// bottom as it does in any long session; then two approvals in one turn:
/// `seq 1 3`, then `seq 4 6`.
fn two_asks() -> ModalSession {
    let filler = (1..=MODAL_ROWS)
        .map(|row| format!("- FILL-{row:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    modal_session_with(vec![
        Reply::Sse(text(&filler)),
        Reply::Sse(tool_call("call-0", SEQ_ASK.0, SEQ_ASK.1)),
        Reply::Sse(tool_call("call-1", SEQ_FOUR.0, SEQ_FOUR.1)),
        Reply::Sse(text(TOOL_TURN_DONE)),
    ])
}

fn rows_with(screen: &[String], needle: &str) -> usize {
    screen.iter().filter(|row| row.contains(needle)).count()
}

// Catches: an answered approval still looks pending when the next tool asks.
#[test]
fn answered_permission_collapses_before_the_next_request() {
    let mut session = two_asks();
    session.press(b"fill the screen\r");
    session.wait(&format!("FILL-{MODAL_ROWS:02}"));
    session.press(b"use the tools\r");
    session.wait("Allow run seq 1 3?");
    session.press(b"y");
    session.wait("Allow run seq 4 6?");
    let screen = session.screen();
    let shown = screen.join("\n");
    assert_eq!(
        rows_with(&screen, "[y] yes"),
        1,
        "only the live approval may show answer keys:\n{shown}"
    );
    assert_eq!(
        rows_with(&screen, "Allow run seq 1 3?"),
        0,
        "the answered approval still reads as a question:\n{shown}"
    );
    session.press(b"y");
    session.wait(TOOL_TURN_DONE);
    let (generations, _) = session.finish();
    assert_eq!(generations.len(), 4, "both allowed calls ran");
}
