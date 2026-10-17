use super::*;

// Catches: /mode focuses Plan instead of the effective mode, so Enter changes policy.
#[test]
fn mode_picker_keeps_the_effective_mode_on_enter() {
    for cols in [80, 120] {
        for (mode, number, label) in [
            ("default", 2, "Default"),
            ("yolo", 5, "Yolo"),
            ("auto", 3, "Auto"),
            ("edits", 4, "Edits"),
            ("plan", 1, "Plan"),
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
