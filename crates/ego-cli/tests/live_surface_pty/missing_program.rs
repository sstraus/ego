use super::*;

// Catches: a missing program reported as a changed execution target after the
// human approved it, and a second approval prompt for the same call.
#[test]
fn an_approved_exec_of_an_absent_program_is_refused_as_not_found() {
    // The directory exists and the file does not, as with the live case
    // `…/mbx/bin/mbx doctor`.
    let program = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mbx");
    assert!(!Path::new(program).exists(), "{program} must be absent");
    let args = format!(r#"{{"argv":["{program}","doctor"],"timeout_ms":5000}}"#);
    let mut session = modal_session(("exec", &args));
    session.press(b"use the tools\r");
    session.wait("[d] details");
    session.press(b"y");
    session.wait(TOOL_TURN_DONE);

    let (generations, _) = session.finish();
    assert_eq!(generations.len(), 2, "one approval settles the call");
    let result = generations[1]["messages"]
        .as_array()
        .expect("provider messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .expect("the refused call's tool result");
    assert!(
        result.contains("program not found") && result.contains(program),
        "the refusal must name the missing program: {result}"
    );
    assert!(!result.contains("captured authority"), "{result}");
}
