use super::*;

// Catches: /setup still enters provider-backed diagnostics instead of a human settings menu.
#[test]
fn slash_setup_opens_settings_without_model_generation_and_esc_keeps_config() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    let original = "model='ollama/qwen3'\nmode='plan'\n";
    std::fs::write(places.home_path().join("config.toml"), original).unwrap();
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--yolo",
        "--trust-workspace",
    ]);
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, None);
    let mut output = Vec::new();
    until_prompt_from(&reader, &mut output, 0);
    let start = output.len();
    input.write_all(b"/setup\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Choose where to save");
    input.write_all(b"\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Setup · Settings");
    input.write_all(b"\x1b[B\x1b[B\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "effective: yolo");
    let return_start = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, return_start, "Setup · Settings");
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(
        std::fs::read_to_string(places.home_path().join("config.toml")).unwrap(),
        original
    );
    assert!(stub.generations().is_empty());
}
