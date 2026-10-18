use super::*;

// Catches: terminal redraws rerun external commands or cache unsafe terminal controls.
#[test]
fn status_provider_runs_once_per_epoch_not_on_help_or_resize() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# status provider fixture\n");
    let marker = ego.workspace_path().join("provider-runs");
    let command = format!(
        "printf x >> {}; printf 'PROVIDER-VALUE\\033[2J'",
        marker.display()
    );
    let argv = serde_json::to_string(&["/bin/sh", "-c", &command]).unwrap();
    ego.home_file(
        "config.toml",
        &format!("[ui.statusline.providers.build]\nargv={argv}\ntimeout_ms=1000\n"),
    );
    let (session, mut input, reader) = PtySession::start(&ego, 24, 160, Some((24, 160)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "build=PROVIDER-VALUE");
    until_prompt_from(&reader, &mut output, start);
    assert_eq!(std::fs::read(&marker).unwrap(), b"x");
    let resize_start = output.len();
    session
        .pair
        .master
        .resize(PtySize {
            rows: 24,
            cols: 150,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    until_normalized(&reader, &mut output, resize_start, "build=PROVIDER-VALUE");
    let help_start = output.len();
    input.write_all(b"/help\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, help_start, "commands");
    until_prompt_from(&reader, &mut output, help_start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(std::fs::read(&marker).unwrap(), b"x");
    assert!(
        stub.generations().is_empty(),
        "status must not call a model"
    );
}

// Catches: a provider timeout leaves its child running or publishes an uncompleted value.
#[test]
fn status_provider_timeout_kills_the_command_and_keeps_the_composer_usable() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# status timeout fixture\n");
    let marker = ego.workspace_path().join("should-not-finish");
    let command = format!("/bin/sleep 5; printf finished > {}", marker.display());
    let argv = serde_json::to_string(&["/bin/sh", "-c", &command]).unwrap();
    ego.home_file(
        "config.toml",
        &format!("[ui.statusline.providers.slow]\nargv={argv}\ntimeout_ms=20\n"),
    );
    let (session, mut input, reader) = PtySession::start(&ego, 24, 120, Some((24, 120)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "execution failed or timed out");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(!marker.exists());
    assert!(stub.generations().is_empty());
}

// Catches: the status producer bypasses the actual session's plan-mode execution floor.
#[test]
fn status_provider_obeys_plan_policy_without_spawning() {
    let stub = Stub::start(Plan::default());
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "plan",
        ])
        .env("TERM", "xterm-256color");
    ego.file("AGENTS.md", "# status policy fixture\n");
    let marker = ego.workspace_path().join("must-not-run");
    let command = format!("printf ran > {}", marker.display());
    let argv = serde_json::to_string(&["/bin/sh", "-c", &command]).unwrap();
    ego.home_file(
        "config.toml",
        &format!("[ui.statusline.providers.denied]\nargv={argv}\n"),
    );
    let (session, mut input, reader) = PtySession::start(&ego, 24, 120, Some((24, 120)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "status provider denied: permission denied",
    );
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(!marker.exists());
    assert!(stub.generations().is_empty());
}
