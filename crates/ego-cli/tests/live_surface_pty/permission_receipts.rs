use super::*;

const SEQ_FOUR: (&str, &str) = ("exec", r#"{"argv":["seq","4","6"],"timeout_ms":5000}"#);

const FILLED: &str = "FILL-40";

/// A session whose first turn fills the screen, so every later modal sits
/// flush at the bottom as it does in any long session. The model then answers
/// with `replies`.
fn filled_session(replies: Vec<Reply>) -> ModalSession {
    let filler = (1..=MODAL_ROWS)
        .map(|row| format!("- FILL-{row:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut completions = vec![Reply::Sse(text(&filler))];
    completions.extend(replies);
    let mut session = modal_session_with(completions);
    session.press(b"fill the screen\r");
    session.wait(FILLED);
    // The text is on screen before the composer turns idle; a resize in
    // between would race the composer's own change of height.
    idle_screen(&mut session);
    session
}

/// Two approvals in one turn: `seq 1 3`, then `seq 4 6`.
fn two_asks() -> ModalSession {
    filled_session(vec![
        Reply::Sse(tool_call("call-0", SEQ_ASK.0, SEQ_ASK.1)),
        Reply::Sse(tool_call("call-1", SEQ_FOUR.0, SEQ_FOUR.1)),
        Reply::Sse(text(TOOL_TURN_DONE)),
    ])
}

/// The screen once the turn has ended and the composer is idle again: the
/// end text is printed while the busy composer is still on screen. The idle
/// composer is the status row over the `>` row; the busy one has the
/// thinking row there instead.
fn idle_screen(session: &mut ModalSession) -> Vec<String> {
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    loop {
        let screen = session.screen();
        let mut rows = screen.iter().rev().filter(|row| !row.trim().is_empty());
        if rows.next().is_some_and(|row| row.starts_with('>'))
            && rows.next().is_some_and(|row| row.contains(" · ctx "))
        {
            return screen;
        }
        assert!(
            Instant::now() < deadline,
            "the composer never became idle:\n{}",
            screen.join("\n")
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn rows_with(screen: &[String], needle: &str) -> usize {
    screen.iter().filter(|row| row.contains(needle)).count()
}

/// The settled screen: no answer keys, no answered question, no stale busy
/// row, each receipt on one row of its own, and the transcript above intact.
fn assert_settled(name: &str, screen: &[String], receipts: &[&str]) {
    let shown = screen.join("\n");
    for stale in ["[y] yes", "Allow run seq", "Esc cancels"] {
        assert_eq!(
            rows_with(screen, stale),
            0,
            "{name}: {stale:?} is left:\n{shown}"
        );
    }
    for receipt in receipts {
        assert_eq!(
            screen
                .iter()
                .filter(|row| row.trim_end() == *receipt)
                .count(),
            1,
            "{name}: not one receipt row {receipt:?}:\n{shown}"
        );
    }
    assert_eq!(
        rows_with(screen, FILLED),
        1,
        "{name}: the transcript was erased:\n{shown}"
    );
}

// Catches: an answered approval still looks pending when the next tool asks.
#[test]
fn answered_permission_collapses_before_the_next_request() {
    let mut session = two_asks();
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
    assert_settled(
        "two asks",
        &idle_screen(&mut session),
        &["Allowed run seq 1 3", "Allowed run seq 4 6"],
    );
    let (generations, _) = session.finish();
    assert_eq!(generations.len(), 4, "both allowed calls ran");
}

/// Keys to press, and what appears once they are handled.
type Step = (&'static [u8], &'static str);

// Catches: one answer path (details, deny, amend, Esc, remember) leaves its
// modal rows on screen or commits no receipt, while plain `y` collapses.
#[test]
fn every_answer_settles_into_its_own_receipt() {
    let cases: [(&str, &[Step], &[&str], &str); 6] = [
        (
            "yes",
            &[(b"y", TOOL_TURN_DONE)],
            &["Allowed run seq 1 3"],
            "✓ $ seq 1 3",
        ),
        (
            "remember",
            &[(b"a", TOOL_TURN_DONE)],
            &["Allowed run seq 1 3 · Remembered: you will not be asked again for exec(seq 1 3)."],
            "✓ $ seq 1 3",
        ),
        (
            "details",
            &[(b"d", "Parameters:"), (b"y", TOOL_TURN_DONE)],
            &["Allowed run seq 1 3"],
            "✓ $ seq 1 3",
        ),
        (
            "deny",
            &[
                (b"n", "What should it do instead?"),
                (b"count with a loop instead\r", TOOL_TURN_DONE),
            ],
            &["Denied run seq 1 3; instead: count with a loop instead"],
            "",
        ),
        (
            "escape",
            &[(b"\x1b", "permission was refused")],
            &["Denied run seq 1 3 · Interrupted by user"],
            "",
        ),
        (
            "edit",
            &[
                (b"e", "argv: "),
                (b"\x15seq 1 4\r", "cwd: "),
                (b"\r", "timeout_ms: "),
                (b"\r", "background: "),
                (b"false\r", "Allow run seq 1 4?"),
                (b"y", TOOL_TURN_DONE),
            ],
            &["Edited run seq 1 3", "Allowed run seq 1 4"],
            // The tool row keeps the original call's label; its output is
            // the edited call's. That label is not this test's subject.
            "✓ $ seq",
        ),
    ];
    for (name, steps, receipts, ran) in cases {
        let mut session = filled_session(vec![
            Reply::Sse(tool_call("call-0", SEQ_ASK.0, SEQ_ASK.1)),
            Reply::Sse(text(TOOL_TURN_DONE)),
        ]);
        session.press(b"use the tools\r");
        session.wait("[d] details");
        for (keys, next) in steps {
            session.press(keys);
            session.wait(next);
        }
        let screen = idle_screen(&mut session);
        let shown = screen.join("\n");
        for form in [
            "Parameters:",
            "What should it do instead?",
            "argv: ",
            "background: ",
        ] {
            assert_eq!(
                rows_with(&screen, form),
                0,
                "{name}: {form:?} is left:\n{shown}"
            );
        }
        assert_settled(name, &screen, receipts);
        assert_eq!(
            screen
                .iter()
                .filter(|row| row.starts_with("✓ $ seq"))
                .count(),
            usize::from(!ran.is_empty()),
            "{name}: the tool row does not match the answer:\n{shown}"
        );
        if !ran.is_empty() {
            assert!(
                screen.iter().any(|row| row.starts_with(ran)),
                "{name}: {ran:?} is missing:\n{shown}"
            );
        }
        // Esc interrupts the turn as well; like the other Esc tests, only
        // the screen is asserted for it, not how the session exits.
        let (status, _, _) = session.quit();
        assert!(
            status.success() || name == "escape",
            "{name}: the session failed: {status:?}"
        );
    }
}

// Catches: a resize while the modal is up, or a width that wraps its rows,
// leaves the answered modal behind because its footprint went stale.
#[test]
fn answered_permission_collapses_after_a_resize_and_when_wrapped() {
    // Only the width changes: the emulator drops bottom rows on a shrink and
    // pads on a growth, where a real terminal scrolls, so a height change
    // would test the emulator rather than ego.
    for (name, before, during) in [
        ("resized while asking", None, Some((MODAL_ROWS, 100))),
        ("wrapped", Some((MODAL_ROWS, 44)), None),
    ] {
        let mut session = two_asks();
        if let Some((rows, cols)) = before {
            session.resize(rows, cols);
            session.wait("qwen3");
        }
        session.press(b"use the tools\r");
        session.wait("[d] details");
        if let Some((rows, cols)) = during {
            session.resize(rows, cols);
            session.wait("[d] details");
        }
        session.press(b"y");
        session.wait("seq 4 6?");
        let screen = session.screen();
        let shown = screen.join("\n");
        assert_eq!(
            rows_with(&screen, "[y] yes"),
            1,
            "{name}: only the live approval may show answer keys:\n{shown}"
        );
        session.press(b"y");
        session.wait(TOOL_TURN_DONE);
        assert_settled(
            name,
            &idle_screen(&mut session),
            &["Allowed run seq 1 3", "Allowed run seq 4 6"],
        );
        session.finish();
    }
}

// Catches: erasing the suspended composer for the modal loses the draft the
// user typed while the model was busy, or paints it twice.
#[test]
fn a_draft_typed_before_the_approval_returns_once_after_it() {
    let (gated, gate) = gated_paced_tool_call_line("call-0", SEQ_ASK.0, SEQ_ASK.1);
    let mut session = filled_session(vec![
        gated,
        Reply::Sse(text(TOOL_TURN_DONE)),
        Reply::Sse(text("DRAFT-RECEIVED")),
    ]);
    // The provider holds the tool call, so the turn stays busy while the
    // draft is typed.
    session.press(b"use the tools\r");
    session.wait("thinking");
    session.press(b"draft kept aside");
    session.wait("draft kept aside");
    gate.release();
    session.wait("[d] details");
    session.press(b"y");
    session.wait(TOOL_TURN_DONE);
    let screen = idle_screen(&mut session);
    let shown = screen.join("\n");
    assert_settled("draft", &screen, &["Allowed run seq 1 3"]);
    assert_eq!(
        rows_with(&screen, "draft kept aside"),
        1,
        "the draft is not back exactly once:\n{shown}"
    );
    session.press(b"\r");
    session.wait("DRAFT-RECEIVED");
    let (generations, _) = session.finish();
    assert!(
        generations
            .last()
            .expect("the draft turn")
            .to_string()
            .contains("draft kept aside"),
        "the restored draft was not what was sent"
    );
}
