use super::*;

// Catches: /mode focuses Plan instead of the effective mode, so Enter changes policy.
#[test]
fn mode_picker_keeps_the_effective_mode_on_enter() {
    for cols in [80, 120] {
        for (mode, number, label) in [
            ("default", 2, "default"),
            ("yolo", 5, "yolo (bypass permissions)"),
            ("auto", 3, "auto"),
            ("accept-edits", 4, "accept-edits"),
            ("plan", 1, "plan"),
        ] {
            let stub = Stub::start(Plan::default());
            let ego = Ego::against(&stub).conversing().args([
                "--model",
                "ollama/qwen3",
                "--mode",
                mode,
                "--sandbox",
                "off",
            ]);
            if mode == "auto" {
                ego.home_file("config.toml", "[permissions]\njudge = \"ollama/qwen3\"\n");
            }
            let mut session = modal_session_on(ego, stub);
            session.resize(MODAL_ROWS, cols);
            session.press(b"/mode\r");
            session.wait("Enter answers; Esc cancels.");
            let shown = session.screen().join("\n");
            session.press(b"\r");
            session.wait("mode `");
            drain_until_quiet(&session.reader, &mut session.output);
            let accepted = normalized(&session.output[session.mark..]);
            let (generations, _) = session.finish();
            assert!(generations.is_empty(), "local picker started a model turn");
            assert!(
                shown.contains(&format!("> {number}. {label} ✓ current")),
                "{cols} columns, {mode}: effective mode is not focused and marked:\n{shown}"
            );
            assert!(
                accepted.contains(&format!("mode `{mode}` is already selected")),
                "Enter changed the effective mode: {accepted}"
            );
        }
    }
}

// Catches: consecutive idle cycle keys submit/erase the draft or change a picker behind the operator.
#[test]
fn shift_tab_retains_the_real_draft_and_is_ignored_by_the_mode_picker() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--mode",
        "default",
        "--sandbox",
        "off",
    ]);
    ego.home_file("config.toml", "[permissions]\njudge='ollama/qwen3'\n");
    let mut session = modal_session_on(ego, stub);
    session.press(b"REGRESSION-MARKER-DRAFT\x1b[Z");
    session.wait("mode `auto` staged");
    session.press(b"\x1b[Z");
    session.wait("mode `accept-edits` staged");
    let shown = session.screen().join("\n");
    assert!(shown.contains("REGRESSION-MARKER-DRAFT"), "draft lost: {shown}");
    assert!(
        session.stub.generations().is_empty(),
        "cycling submitted a model turn"
    );
    session.press(b"\x15/mode\r");
    session.wait("Enter answers; Esc cancels.");
    session.press(b"\x1b[Z\r");
    session.wait("mode `accept-edits` is already selected");
    let (generations, _) = session.finish();
    assert!(generations.is_empty(), "picker keys submitted a model turn");
}

// Catches: two busy keys resolve from stale editor mode instead of owner queue order.
#[test]
fn queued_shift_tab_cycles_preserve_draft_and_compose_after_a_direct_selection() {
    let (reply, gate) = gated_paced_line("REGRESSION-MARKER-RUNNING");
    let stub = Stub::start(Plan {
        completions: vec![reply, Reply::Sse(text("REGRESSION-MARKER-DRAFT-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--mode",
        "default",
        "--sandbox",
        "off",
    ]);
    ego.home_file("config.toml", "[permissions]\njudge='ollama/qwen3'\n");
    let mut session = modal_session_on(ego, stub);
    session.press(b"hold turn\r");
    session.wait("REGRESSION-MARKER-RUNNING");
    // Direct selection and cycles are queued while the generation is fixed at default.
    session.press(b"/mode plan\r");
    session.press(b"REGRESSION-MARKER-BUSY-DRAFT\x1b[Z\x1b[Z");
    assert_eq!(
        session.stub.generations().len(),
        1,
        "local commands submitted a prompt"
    );
    gate.release();
    session.wait("mode `auto` staged");
    let shown = session.screen().join("\n");
    assert!(
        shown.contains("REGRESSION-MARKER-BUSY-DRAFT"),
        "busy draft lost: {shown}"
    );
    session.press(b"\r");
    session.wait("REGRESSION-MARKER-DRAFT-ANSWER");
    let (generations, _) = session.finish();
    assert_eq!(generations.len(), 2);
    assert!(
        generations[1].to_string().contains("REGRESSION-MARKER-BUSY-DRAFT"),
        "retained draft did not reach next generation"
    );
}
