use super::*;

// Catches: the real submission path echoes the whole brief, loses model input,
// or cannot restore the hidden text through /expand.
#[test]
fn long_prompt_echo_is_collapsed_in_the_live_terminal() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("REGRESSION-MARKER-ANSWER"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    ego.file("AGENTS.md", "# prompt echo fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 120, Some((40, 120)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let prompt = (1..=20)
        .map(|n| format!("brief line {n:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let start = output.len();
    input.write_all(b"\x1b[200~").unwrap();
    input.write_all(prompt.as_bytes()).unwrap();
    input.write_all(b"\x1b[201~\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "REGRESSION-MARKER-ANSWER");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let collapsed = screen_rows(&output, 40, 120).join("\n");
    assert!(collapsed.contains("> brief line 01"), "{collapsed}");
    assert!(
        collapsed.contains("17 more lines · /expand shows all"),
        "{collapsed}"
    );
    assert!(!collapsed.contains("brief line 20"), "{collapsed}");
    assert_eq!(
        count_exact_strings(&stub.generations()[0]["messages"], &prompt),
        1
    );
    let start = output.len();
    input.write_all(b"/expand\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "brief line 20");
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let expanded = screen_rows(&output, 40, 120).join("\n");
    for n in 1..=20 {
        assert!(
            expanded.contains(&format!("brief line {n:02}")),
            "{expanded}"
        );
    }
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
}
