use super::*;

// Catches: slash setup bypasses its terminal confirmation or cannot publish a host-approved candidate.
#[test]
fn setup_slash_real_terminal_apply_preserves_captured_session_authority() {
    setup_slash_case(b"yes\r", true);
}

// Catches: cancelling slash setup publishes the prepared candidate.
#[test]
fn setup_slash_real_terminal_cancel_keeps_config_bytes() {
    setup_slash_case(b"\x1b", false);
}

fn setup_slash_case(answer: &[u8], applied: bool) {
    let needed = tempfile::tempdir().unwrap();
    let target = needed.path().join("task.md");
    std::fs::write(&target, "outside task").unwrap();
    let selection =
        serde_json::json!([{"kind":"read", "path":target, "reason":"task"}]).to_string();
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
    until_normalized(&reader, &mut output, start, "Apply? [y/N]");
    assert_eq!(
        std::fs::read_to_string(places.home_path().join("config.toml")).unwrap(),
        original
    );
    let start = output.len();
    input.write_all(answer).unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        if applied {
            "Setup publication"
        } else {
            "Setup declined/cancelled"
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
    assert_eq!(std::fs::read_to_string(target).unwrap(), "outside task");
    assert_eq!(
        stub.generations().len(),
        1,
        "slash setup must not start ordinary generation"
    );
}
