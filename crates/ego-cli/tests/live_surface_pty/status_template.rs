use super::*;

// Catches template rows being hard-coded above the buffer, or footer repaint overwriting edits.
#[test]
fn status_template_positions_rows_around_editable_prompt_in_real_pty() {
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
    ego.file("AGENTS.md", "# status template fixture\n");
    ego.home_file("config.toml", "[ui.statusline]\ntemplate = '''TEMPLATE-ABOVE {{ model }}\n{{ prompt }}\nTEMPLATE-BELOW {{ effort }}'''\n");
    let (session, mut input, reader) = PtySession::start(&ego, 24, 100, Some((24, 100)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "TEMPLATE-BELOW off");
    input.write_all(b"editable-probe").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "editable-probe");
    drain_until_quiet(&reader, &mut output);
    let mut parser = vt100::Parser::new(24, 100, 0);
    parser.process(&output);
    let screen = parser.screen();
    let rows = screen.rows(0, 100).collect::<Vec<_>>();
    let above = rows
        .iter()
        .position(|row| row.contains("TEMPLATE-ABOVE ollama/qwen3"))
        .unwrap();
    let prompt = rows
        .iter()
        .position(|row| row.starts_with("> editable-probe"))
        .unwrap();
    let below = rows
        .iter()
        .position(|row| row.contains("TEMPLATE-BELOW off"))
        .unwrap();
    assert_eq!((prompt, below), (above + 1, above + 2), "{rows:?}");
    assert_eq!(screen.cursor_position().0 as usize, prompt, "{rows:?}");
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(stub.generations().is_empty());
}

// Catches a footer as tall as the terminal painting over the draft and saved cursor.
#[test]
fn status_template_footer_does_not_overwrite_composer_at_terminal_height() {
    for (height, footer, expected_footer) in [
        (
            4,
            "FOOTER-ONE\nFOOTER-TWO\nFOOTER-THREE\nFOOTER-FOUR",
            vec!["FOOTER-ONE", "FOOTER-TWO", ""],
        ),
        (
            4,
            "FOOTER-ONE\nFOOTER-TWO\nFOOTER-THREE",
            vec!["FOOTER-ONE", "FOOTER-TWO", ""],
        ),
        (
            1,
            "FOOTER-ONE\nFOOTER-TWO\nFOOTER-THREE\nFOOTER-FOUR",
            vec![],
        ),
    ] {
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
        ego.file("AGENTS.md", "# status template fixture\n");
        ego.home_file(
            "config.toml",
            &format!("[ui.statusline]\ntemplate = '''{{{{ prompt }}}}\n{footer}'''\n"),
        );
        let (session, mut input, reader) = PtySession::start(&ego, height, 80, Some((height, 80)));
        let mut output = Vec::new();
        until(&reader, &mut output, b"Trust project files");
        let start = output.len();
        input.write_all(b"yes\r").unwrap();
        input.flush().unwrap();
        if height > 1 {
            until_normalized(&reader, &mut output, start, "FOOTER-ONE");
        } else {
            // The prose normalizer removes leading composer markers.
            until_prompt_from(&reader, &mut output, start);
        }
        input.write_all(b"editable-probe").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "editable-probe");
        drain_until_quiet(&reader, &mut output);
        let mut parser = vt100::Parser::new(height, 80, 0);
        parser.process(&output);
        let screen = parser.screen();
        let rows = screen.rows(0, 80).collect::<Vec<_>>();
        let cursor = screen.cursor_position();
        input.write_all(b"\x15/quit\r").unwrap();
        input.flush().unwrap();
        assert!(session.quit(input).success());
        assert_eq!(rows[0], "> editable-probe", "height={height} {rows:?}");
        assert_eq!(cursor.0, 0, "height={height} {rows:?}");
        assert_eq!(
            rows[1..]
                .iter()
                .map(|row| row.trim_end())
                .collect::<Vec<_>>(),
            expected_footer,
            "height={height}: footer must leave the host overlay row blank"
        );
        assert!(stub.generations().is_empty());
    }
}

// Catches diagnostics suppressed by the terminal adapter, duplicated on repaint,
// or reissued after a modal retires the notice for the same configuration.
#[test]
fn status_template_diagnostic_is_visible_once_across_live_repaints() {
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
    ego.file("AGENTS.md", "# diagnostic fixture\n");
    ego.home_file(
        "config.toml",
        "[ui.statusline]\ntemplate = '''{{ unknown }}\n{{ prompt }}'''\n",
    );
    let (session, mut input, reader) = PtySession::start(&ego, 24, 120, Some((24, 120)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "statusline:");
    let mut screens = Vec::new();
    for _ in 0..3 {
        let start = output.len();
        // Ctrl+L redraws without dismissing the notice; printable input intentionally does.
        input.write_all(b"\x0c").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "statusline:");
        until_notice_prose(&reader, &mut output, start, "(line 1)");
        let mut parser = vt100::Parser::new(24, 120, 0);
        parser.process(&output);
        screens.push(parser.screen().contents());
    }
    let start = output.len();
    input.write_all(b"\x15/model\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Select model");
    drain_until_quiet(&reader, &mut output);
    let start = output.len();
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"after-modal-repaint").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "after-modal-repaint");
    drain_until_quiet(&reader, &mut output);
    let after_modal = normalized(&output[start..]);
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    for screen in screens {
        assert_eq!(screen.matches("statusline:").count(), 1, "{screen}");
        assert!(screen.contains("(line 1)"), "{screen}");
    }
    assert!(!after_modal.contains("statusline:"), "{after_modal}");
    assert!(stub.generations().is_empty());
}

// Catches a saved key being left out of the already-running picker, a stale
// statusline or generation route after confirmation, and a rebind after cancelling a fork.
#[cfg(unix)]
#[test]
fn model_picker_admits_external_gemini_login_and_routes_next_requests() {
    const KEY: &str = "synthetic-external-login-key";
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400)],
        ..Plan::default()
    });
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
        .env("TERM", "xterm-256color")
        .env("OPENAI_API_KEY", "fixture-openai-key")
        .env("GEMINI_BASE_URL", &format!("http://{}", stub.address));
    ego.file("AGENTS.md", "# external login fixture\n");
    let config_path = ego.home_file(
        "config.toml",
        "model = 'ollama/qwen3'\n[ui.statusline]\ntemplate = '''BOUND {{ model }}\n{{ prompt }}'''\n",
    );
    let (session, mut input, reader) = PtySession::start(&ego, 30, 140, Some((30, 140)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, start);

    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_ego"));
    command.env_clear();
    command.env("HOME", ego.home_path());
    command.env("EGO_HOME", ego.home_path());
    command.cwd(ego.workspace_path());
    command.args(["auth", "login", "gemini"]);
    let (login, mut login_input, login_reader) = PtySession::start_command(command, 24, 120, None);
    let mut login_output = Vec::new();
    until(&login_reader, &mut login_output, b"(input hidden): ");
    login_input
        .write_all(format!("\x1b[200~{KEY}\x1b[201~\r").as_bytes())
        .unwrap();
    login_input.flush().unwrap();
    until(
        &login_reader,
        &mut login_output,
        b"logged in gemini account ",
    );
    assert!(login.quit(login_input).success());
    assert!(!normalized(&login_output).contains(KEY));

    let config_after_login = std::fs::read_to_string(&config_path).unwrap();
    let config =
        ego_core::config::ConfigLayer::parse(ego_core::config::Source::User, &config_after_login)
            .unwrap()
            .value;
    let (id, account) = config.connections.iter().next().unwrap();
    assert_eq!(account.vendor, "gemini");
    assert!(config.connection.is_none());
    assert!(config_after_login.contains("ollama/qwen3"));

    for model in ["gemini-3.8-flash", "gemini-3.1-pro-preview"] {
        let start = output.len();
        input.write_all(b"/model\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "Select model");
        let start = output.len();
        input.write_all(format!("{model}\r").as_bytes()).unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            start,
            &format!("Select reasoning for gemini/{model}"),
        );
        let start = output.len();
        input.write_all(b"1s").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            start,
            &format!("BOUND gemini/{model}"),
        );
        until_post_trust_prompt(&reader, &mut output, start);
        let start = output.len();
        input.write_all(b"Verify the selected model.\r").unwrap();
        input.flush().unwrap();
        until_normalized(
            &reader,
            &mut output,
            start,
            "the provider returned HTTP 400",
        );
        until_normalized(&reader, &mut output, start, "Turn elapsed");
        until_post_trust_prompt(&reader, &mut output, start);
    }
    let start = output.len();
    input.write_all(b"/model openai/gpt-6-luna\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Your next prompt will share");
    let start = output.len();
    input.write_all(b"no\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "provider switch cancelled");
    until_post_trust_prompt(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"\x0c").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "BOUND gemini/gemini-3.1-pro-preview",
    );
    let start = output.len();
    input
        .write_all(b"Verify the cancelled switch did not change routing.\r")
        .unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "the provider returned HTTP 400",
    );
    until_post_trust_prompt(&reader, &mut output, start);
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let transcript = normalized(&output);
    assert!(!transcript.contains(KEY));
    assert!(!transcript.contains(id));
    assert!(
        stub.generations().is_empty(),
        "a prompt reached Ollama or the cancelled OpenAI destination"
    );
    let requests = stub.requests();
    let sent: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/interactions"))
        .map(|request| {
            let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
            assert_eq!(
                request.headers.get("x-goog-api-key").map(String::as_str),
                Some(KEY)
            );
            body["model"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        sent,
        [
            "gemini-3.8-flash",
            "gemini-3.1-pro-preview",
            "gemini-3.1-pro-preview"
        ]
    );
    assert_eq!(
        std::fs::read_to_string(config_path).unwrap(),
        config_after_login
    );
}

// Catches: a live picker guesses an account or exposes opaque connection IDs.
#[cfg(unix)]
#[test]
fn model_picker_offers_a_numbered_account_choice_for_a_second_gemini_login() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400)],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
            "--session-id",
            support::FIXTURE_SESSION_ID,
        ])
        .env("TERM", "xterm-256color")
        .env("GEMINI_BASE_URL", &format!("http://{}", stub.address));
    ego.file("AGENTS.md", "# second account fixture\n");
    let config_path = ego.home_file("config.toml", "model = 'ollama/qwen3'\n");
    let (session, mut input, reader) = PtySession::start(&ego, 30, 140, Some((30, 140)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, start);

    // Two plain logins against the same provider always create two distinct,
    // numbered accounts; neither flag renews or targets the other.
    for key in ["first-account-key", "second-account-key"] {
        login_fixture_gemini_account(&ego, key);
    }

    let config_after_logins = std::fs::read_to_string(&config_path).unwrap();
    let config =
        ego_core::config::ConfigLayer::parse(ego_core::config::Source::User, &config_after_logins)
            .unwrap()
            .value;
    let gemini_ids = config
        .connections
        .iter()
        .filter(|(_, connection)| connection.vendor == "gemini")
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    assert_eq!(gemini_ids.len(), 2, "{config_after_logins}");

    // This session never generated with any gemini account: picking a
    // gemini model must ask which numbered account to use rather than
    // guess, and the two rows must never leak either connection ID.
    let start = output.len();
    input.write_all(b"/model\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Select model");
    let start = output.len();
    input.write_all(b"gemini-3.8-flash\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Select account for gemini/gemini-3.8-flash",
    );
    let chooser = normalized(&output[start..]);
    assert!(chooser.contains("1. account 1"), "{chooser}");
    assert!(chooser.contains("2. account 2"), "{chooser}");
    for id in &gemini_ids {
        assert!(!chooser.contains(id.as_str()), "{chooser}");
    }

    let start = output.len();
    input.write_all(b"2\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Select reasoning for gemini/gemini-3.8-flash",
    );
    let start = output.len();
    input.write_all(b"s").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Model: gemini/gemini-3.8-flash",
    );
    until_post_trust_prompt(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"Check the chosen account.\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "the provider returned HTTP 400",
    );
    until_post_trust_prompt(&reader, &mut output, start);
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    // Do not pass either --account or --model: both must come from the
    // accepted picker selection, not the on-disk Ollama default or the
    // first of the two now-available Gemini accounts.
    let resumed = Ego::at(ego.home_path(), ego.workspace_path(), Some(&stub))
        .env("GEMINI_BASE_URL", &format!("http://{}", stub.address))
        .subcommand("resume")
        .args([
            support::FIXTURE_SESSION_ID,
            "--sandbox",
            "off",
            "--output",
            "json",
            "Verify the picker account survived resume.",
        ])
        .run();
    assert!(
        resumed.stderr.contains("the provider returned HTTP 400"),
        "{}",
        resumed.stderr
    );
    let requests = stub.requests();
    let sent: Vec<_> = requests
        .iter()
        .filter(|request| request.path.ends_with("/interactions"))
        .collect();
    assert_eq!(sent.len(), 2);
    for request in sent {
        assert_eq!(
            request.headers.get("x-goog-api-key").map(String::as_str),
            Some("second-account-key")
        );
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        assert_eq!(body["model"], "gemini-3.8-flash");
    }
    assert_eq!(
        std::fs::read_to_string(config_path).unwrap(),
        config_after_logins
    );
}

// Catches: confirmation publishes an ambiguous route or sends the next turn there.
#[test]
fn model_picker_refuses_ambiguous_captured_accounts_and_keeps_wire_route() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Status(400)],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/claude-opus-5-5",
            "--thinking",
            "low",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("TERM", "xterm-256color")
        .env("ANTHROPIC_API_KEY", "synthetic-original-route-key")
        .env("GEMINI_API_KEY", "synthetic-ambiguous-route-key")
        .env("GEMINI_BASE_URL", &format!("http://{}", stub.address));
    ego.file("AGENTS.md", "# ambiguous account fixture\n");
    let configuration = "model='anthropic/claude-opus-5-5'\n[connections.a]\nname='A'\nvendor='gemini'\naccess='key'\nkey_env='FIRST_GEMINI_API_KEY'\n[connections.b]\nname='B'\nvendor='gemini'\naccess='key'\nkey_env='SECOND_GEMINI_API_KEY'\n";
    let path = ego.home_file("config.toml", configuration);
    let (session, mut input, reader) = PtySession::start(&ego, 30, 140, Some((30, 140)));
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_post_trust_prompt(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/model\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Select model");
    let start = output.len();
    // Slash starts a command in the modal; use the model's literal fragment.
    input.write_all(b"gemini-3.8-flash\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Select reasoning for gemini/gemini-3.8-flash",
    );
    let start = output.len();
    input.write_all(b"1\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "multiple gemini accounts match",
    );
    // A model-modal repaint may contain a composer before the refusal.
    // Only the idle frame after the refusal permits the follow-up prompt.
    until_prompt_after_reply(&reader, &mut output, b"multiple gemini accounts match");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), configuration);
    let start = output.len();
    input.write_all(b"Confirm the original route.\r").unwrap();
    input.flush().unwrap();
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    while !normalized(&output[start..]).contains("the provider returned HTTP 400") {
        let Ok(chunk) = reader.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        else {
            let paths: Vec<_> = stub
                .requests()
                .into_iter()
                .map(|request| request.path)
                .collect();
            panic!(
                "expected loopback refusal; paths={paths:?}, output={}",
                normalized(&output[start..])
            );
        };
        output.extend(chunk);
    }
    until_post_trust_prompt(&reader, &mut output, start);
    input.write_all(b"\x15/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let requests = stub.requests();
    assert!(
        !requests
            .iter()
            .any(|request| request.path.ends_with("/interactions"))
    );
    let generations = stub.generations();
    assert_eq!(generations.len(), 1);
    assert_eq!(generations[0]["model"], "claude-opus-5-5");
}

fn login_fixture_gemini_account(ego: &Ego, key: &str) {
    let mut command = portable_pty::CommandBuilder::new(env!("CARGO_BIN_EXE_ego"));
    command.env_clear();
    command.env("HOME", ego.home_path());
    command.env("EGO_HOME", ego.home_path());
    command.cwd(ego.workspace_path());
    command.args(["auth", "login", "gemini"]);
    let (login, mut login_input, login_reader) = PtySession::start_command(command, 24, 120, None);
    let mut login_output = Vec::new();
    until(&login_reader, &mut login_output, b"(input hidden): ");
    login_input
        .write_all(format!("\x1b[200~{key}\x1b[201~\r").as_bytes())
        .unwrap();
    login_input.flush().unwrap();
    until(
        &login_reader,
        &mut login_output,
        b"logged in gemini account ",
    );
    assert!(login.quit(login_input).success());
    assert!(!normalized(&login_output).contains(key));
}

// Catches cached child authority refusing an existing account or losing a newly saved one.
#[test]
fn background_task_inherits_live_gemini_picker_account() {
    use ego_core::event::domain::EventKind;
    use ego_core::store::codec::{StoreLine, decode_log};
    use ego_core::store::session_store::SessionEntry;

    let mut failures = Vec::new();
    for captured in [true, false] {
        let result = std::panic::catch_unwind(|| {
            const CHILD: &str = "Print child account proof";
            const PARENT: &str = "Start a background account test";
            const KEY: &str = "picker-child-key";
            // Use the Gemini wire contract fixture, replacing its function payload with Ego's task.
            let wire =
                include_str!("../../../ego-ai/tests/goldens/profile/gemini/stream/tool_calls.sse");
            let (prefix, _) = wire
                .split_once("event: step.start\ndata: {\"index\":1")
                .unwrap();
            let (_, suffix) = wire.split_once("event: interaction.status_update").unwrap();
            let call = serde_json::json!({"index":1,"step":{
                "type":"function_call", "id":"child01", "name":"task",
                "arguments":{}
            },"event_type":"step.start"});
            let arguments = serde_json::json!({"index":1,"delta":{
                "type":"arguments_delta", "arguments":serde_json::json!({"name":"Account proof","prompt":CHILD,"background":true}).to_string()
            },"event_type":"step.delta"});
            let tool = format!(
                "{prefix}event: step.start\ndata: {call}\n\nevent: step.delta\ndata: {arguments}\n\nevent: step.stop\ndata: {{\"index\":1,\"event_type\":\"step.stop\"}}\n\nevent: interaction.status_update{suffix}"
            );
            let text = include_str!("../../../ego-ai/tests/goldens/profile/gemini/stream/text.sse");
            let stub = Stub::start_concurrent(Plan {
                // The first delegation activates the coordinator prompt; the next call actually runs.
                completions: vec![
                    Reply::WireSse(tool.clone()),
                    Reply::WireSse(
                        tool.replace("child01", "child02")
                            .replace("v1_4de1", "v1_child02"),
                    ),
                    Reply::WireSse(text.into()),
                ],
                ..Plan::default()
            });
            let ego = Ego::against(&stub)
                .conversing()
                .args([
                    "--model",
                    "ollama/qwen3",
                    "--sandbox",
                    "off",
                    "--mode",
                    "yolo",
                    "--session-id",
                    support::FIXTURE_SESSION_ID,
                ])
                .env("TERM", "xterm-256color")
                .env("GEMINI_BASE_URL", &format!("http://{}", stub.address));
            ego.file("AGENTS.md", "# child account fixture\n");
            let path = ego.home_file("config.toml", "model = 'ollama/qwen3'\n");
            if captured {
                login_fixture_gemini_account(&ego, "other-account-key");
                login_fixture_gemini_account(&ego, KEY);
            }
            let (session, mut input, reader) = PtySession::start(&ego, 30, 140, Some((30, 140)));
            let mut output = Vec::new();
            until(&reader, &mut output, b"Trust project files");
            let start = output.len();
            input.write_all(b"yes\r").unwrap();
            input.flush().unwrap();
            until_post_trust_prompt(&reader, &mut output, start);
            if !captured {
                login_fixture_gemini_account(&ego, KEY);
            }
            let config = ego_core::config::ConfigLayer::parse(
                ego_core::config::Source::User,
                &std::fs::read_to_string(path).unwrap(),
            )
            .unwrap()
            .value;
            let chosen = config.account_numbers["gemini"]
                .iter()
                .find(|(_, number)| **number == if captured { 2 } else { 1 })
                .unwrap()
                .0;
            let start = output.len();
            input.write_all(b"/model\r").unwrap();
            input.flush().unwrap();
            until_normalized(&reader, &mut output, start, "Select model");
            let start = output.len();
            input.write_all(b"gemini-3.8-flash\r").unwrap();
            input.flush().unwrap();
            if captured {
                until_normalized(
                    &reader,
                    &mut output,
                    start,
                    "Select account for gemini/gemini-3.8-flash",
                );
                input.write_all(b"2\r").unwrap();
                input.flush().unwrap();
            }
            until_normalized(
                &reader,
                &mut output,
                start,
                "Select reasoning for gemini/gemini-3.8-flash",
            );
            let start = output.len();
            input.write_all(b"s").unwrap();
            input.flush().unwrap();
            until_normalized(
                &reader,
                &mut output,
                start,
                "Model: gemini/gemini-3.8-flash",
            );
            until_post_trust_prompt(&reader, &mut output, start);
            let start = output.len();
            write!(input, "{PARENT}\r").unwrap();
            input.flush().unwrap();
            until_normalized(&reader, &mut output, start, "Turn elapsed");
            until_post_trust_prompt(&reader, &mut output, start);
            let parent_journal = ego
                .home_path()
                .join("sessions")
                .join(support::FIXTURE_SESSION_ID)
                .join("events.jsonl");
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                let journal = std::fs::read_to_string(&parent_journal).unwrap();
                if journal.contains("SubagentSettled") {
                    break;
                }
                assert!(
                    journal.contains("BackgroundTaskRequested") && Instant::now() < deadline,
                    "background task never settled; requests: {:?}; journal: {journal}",
                    stub.requests()
                        .iter()
                        .map(|req| &req.body)
                        .collect::<Vec<_>>()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            input.write_all(b"/quit\r").unwrap();
            input.flush().unwrap();
            assert!(session.quit(input).success());

            let sessions = ego.home_path().join("sessions");
            let entries = |id: &str| {
                decode_log(&std::fs::read(sessions.join(id).join("events.jsonl")).unwrap())
                    .unwrap()
                    .into_iter()
                    .filter_map(|line| match line {
                        StoreLine::Record(record) => Some(
                            serde_json::from_str::<SessionEntry>(record.payload.get()).unwrap(),
                        ),
                        _ => None,
                    })
            };
            let child = entries(support::FIXTURE_SESSION_ID)
                .find_map(|entry| match entry {
                    SessionEntry::Event(event) => match event.kind {
                        EventKind::SubagentSpawned { child, .. } => Some(child),
                        _ => None,
                    },
                    _ => None,
                })
                .expect("background task was admitted");
            assert!(entries(&child.session_id().to_string()).any(|entry| match entry {
                SessionEntry::Event(event) => matches!(event.kind, EventKind::SessionSelectionsSelected { selections } if selections.connection() == Some(chosen.as_str())),
                SessionEntry::RunTransaction(run) => run.selections().connection() == Some(chosen.as_str()),
                _ => false,
            }), "child did not persist the selected account");
            let child_request = stub
                .requests()
                .into_iter()
                .find(|request| {
                    request.path.ends_with("/interactions")
                        && request.body.contains(CHILD)
                        && !request.body.contains(PARENT)
                })
                .expect("background task sent a Gemini request");
            assert_eq!(
                child_request
                    .headers
                    .get("x-goog-api-key")
                    .map(String::as_str),
                Some(KEY)
            );
            let body: serde_json::Value = serde_json::from_str(&child_request.body).unwrap();
            assert_eq!(body["model"], "gemini-3.8-flash");
        });
        if let Err(failure) = result {
            failures.push(format!(
                "captured={captured}: {}",
                failure
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| failure.downcast_ref::<&str>().copied())
                    .unwrap_or("non-text panic")
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
