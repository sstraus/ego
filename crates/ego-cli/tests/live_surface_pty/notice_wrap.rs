use super::*;

// Catches: trust prose and startup diagnostics rely on terminal character wrap.
#[test]
fn startup_notices_word_wrap_at_80_columns_without_splitting_words() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .env("TERM", "xterm-256color")
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ]);
    ego.file("AGENTS.md", "# notice wrap fixture\n");
    let (session, mut input, reader) = PtySession::start(&ego, 40, 80, Some((40, 80)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    drain_until_quiet(&reader, &mut output);
    let trust_end = output.len();
    let mut errors = Vec::new();
    let mut trust = vt100::Parser::new(40, 80, 0);
    trust.process(&output);
    let trust_rows: Vec<_> = trust.screen().rows(0, 80).collect();
    let yes = trust_rows
        .iter()
        .position(|row| row.starts_with("Yes: load"))
        .unwrap();
    let no = trust_rows
        .iter()
        .position(|row| row.starts_with("No: continue"))
        .unwrap();
    let sentence = trust_rows[yes..no].join(" ");
    if sentence.split_whitespace().collect::<Vec<_>>()
        != "Yes: load project instructions, configuration, skills, hooks and MCP server declarations."
            .split_whitespace().collect::<Vec<_>>()
    {
        errors.push(format!("trust prose split a word: {sentence:?}"));
    }
    for row in 0..40 {
        if trust.screen().row_wrapped(row) {
            errors.push(format!("trust row {row} relies on terminal auto-wrap"));
        }
    }
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    drain_until_quiet(&reader, &mut output);
    let startup_end = output.len();
    let mut startup = vt100::Parser::new(40, 80, 0);
    startup.process(&output);
    let rows: Vec<_> = startup.screen().rows(0, 80).collect();
    if !rows.iter().any(|row| row.starts_with("εgo:")) {
        errors.push(format!("missing startup diagnostics: {rows:?}"));
    }
    for row in 0..40 {
        if startup.screen().row_wrapped(row) {
            errors.push(format!(
                "startup row {row} relies on terminal auto-wrap: {:?}",
                rows[row as usize]
            ));
        }
    }
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    if let Some(directory) = std::env::var_os("EGO_NOTICE_WRAP_CAPTURE_DIR") {
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("startup-80.ansi"), &output).unwrap();
        std::fs::write(directory.join("startup-80.json"), serde_json::to_vec_pretty(
            &serde_json::json!({"rows": 40, "cols": 80, "trust_end": trust_end, "startup_end": startup_end})
        ).unwrap()).unwrap();
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}
