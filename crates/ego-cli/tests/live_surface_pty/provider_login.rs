use super::*;

// Catches: a typed login is treated as a model prompt or lists helper-only identities.
#[test]
fn login_lists_builtin_oauth_without_generation() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# login fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader, mut output) = login_session(&ego, false);
    let start = output.len();
    input.write_all(b"/login\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "/login PROVIDER — authorize a provider",
    );
    until_prompt_from(&reader, &mut output, start);
    let listing = normalized(&output[start..]);
    assert!(listing.contains("openai-codex"));
    assert!(!listing.contains("anthropic"));
    assert!(!listing.contains("google-cloud"));
    let start = output.len();
    input.write_all(b"/login bogus\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "not a built-in OAuth provider");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(stub.generations().is_empty());
    assert!(!places.home_path().join("auth.json").exists());
}

// Catches: an external task token overrides the stored login despite a success receipt.
#[test]
fn login_refuses_an_external_token_that_would_override_the_store() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# login fixture\n");
    let ego = places
        .ego(&stub)
        .conversing()
        .args([
            "--model",
            "ollama/qwen3",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("OPENAI_CODEX_ACCESS_TOKEN", "fixture-external-token");
    let (session, mut input, reader, mut output) = login_session(&ego, false);
    let start = output.len();
    input.write_all(b"/login openai-codex\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "uses OPENAI_CODEX_ACCESS_TOKEN",
    );
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(stub.generations().is_empty());
    assert!(!places.home_path().join("auth.json").exists());
}

// Catches: Ctrl-C during the loopback wait leaves an open login or writes a
// credential despite no callback from the authorization server.
#[test]
fn browser_login_interrupt_restores_terminal_without_storing_a_token() {
    let mut lock = browser_lock();
    let _guard = lock.write().unwrap();
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# login fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    ego.home_file(
        "config.toml",
        "[auth.anthropic]\noauth_login_enabled = true\n",
    );
    let (session, mut input, reader, mut output) = login_session(&ego, false);
    let start = output.len();
    input.write_all(b"/login anthropic\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Waiting for the loopback callback on this host",
    );
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "login cancelled");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(stub.generations().is_empty());
    assert!(!places.home_path().join("auth.json").exists());
}

// Catches: Esc in the device/browser method selector starts OAuth or leaves
// the terminal unusable instead of cancelling before any network effect.
#[test]
fn login_method_escape_restores_the_prompt_without_credentials() {
    let stub = Stub::start(Plan::default());
    let places = support::Places::new();
    places.file("AGENTS.md", "# login fixture\n");
    let ego = places.ego(&stub).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    let (session, mut input, reader, mut output) = login_session(&ego, false);
    let start = output.len();
    input.write_all(b"/login openai-codex\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Choose an OAuth login method");
    input.write_all(b"\x1b").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "login cancelled");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(stub.generations().is_empty());
    assert!(!places.home_path().join("auth.json").exists());
}

// Catches: saving a new OAuth connection silently switches an existing key-backed session.
#[test]
fn browser_login_saves_a_connection_without_switching_the_current_account() {
    let mut lock = browser_lock();
    let _guard = lock.write().unwrap();
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("OAUTH-PTY-TURN"))],
        ..Plan::default()
    });
    let ego = Ego::against(&stub)
        .conversing()
        .args([
            "--model",
            "anthropic/claude-sonnet-5-5",
            "--sandbox",
            "off",
            "--mode",
            "yolo",
        ])
        .env("ANTHROPIC_API_KEY", "fixture-bootstrap")
        .arg("--thinking=low");
    ego.file("AGENTS.md", "# OAuth PTY fixture\n");
    ego.home_file(
        "config.toml",
        "[auth.anthropic]\noauth_login_enabled = true\n",
    );
    let (session, mut input, reader, mut output) = login_session(&ego, true);
    let start = output.len();
    input.write_all(b"before login\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "OAUTH-PTY-TURN");
    until_prompt_from(&reader, &mut output, start);
    // The turn's status row can outlive the first prompt; login is refused until it ends.
    drain_until_quiet(&reader, &mut output);
    let start = output.len();
    input.write_all(b"/login anthropic\r").unwrap();
    input.flush().unwrap();
    until_normalized(
        &reader,
        &mut output,
        start,
        "Waiting for the loopback callback on this host",
    );
    let receipt = normalized(&output[start..]).replace(['\r', '\n'], "");
    let state = receipt
        .split("state=")
        .nth(1)
        .unwrap()
        .split("εgo:")
        .next()
        .unwrap()
        .split_whitespace()
        .collect::<String>();
    let mut callback = std::net::TcpStream::connect("127.0.0.1:53692").unwrap();
    callback.set_read_timeout(Some(PROGRESS_DEADLINE)).unwrap();
    write!(
        callback,
        "GET /callback?code=fixture-code&state={state} HTTP/1.1\r\nHost: localhost\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    callback.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "callback state={state:?}, response={response:?}, login receipt={receipt:?}"
    );
    until_normalized(&reader, &mut output, start, "Logged in anthropic");
    until_prompt_from(&reader, &mut output, start);
    assert!(ego.home_path().join("auth.json").exists());
    let start = output.len();
    input.write_all(b"after login\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "OAUTH-PTY-TURN");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    let requests: Vec<_> = stub
        .requests()
        .into_iter()
        .filter(|request| request.path.ends_with("/messages"))
        .collect();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .headers
            .get("x-api-key")
            .is_some_and(|value| value == "fixture-bootstrap")
    );
    assert!(
        requests[1]
            .headers
            .get("x-api-key")
            .is_some_and(|value| value == "fixture-bootstrap")
    );
    assert!(!requests[1].headers.contains_key("authorization"));
    let config: toml::Value =
        toml::from_str(&std::fs::read_to_string(ego.home_path().join("config.toml")).unwrap())
            .unwrap();
    let connections = config["connections"].as_table().unwrap();
    assert_eq!(connections.len(), 1);
    let (id, connection) = connections.iter().next().unwrap();
    assert_eq!(connection["vendor"].as_str(), Some("anthropic"));
    assert_eq!(connection["access"].as_str(), Some("oauth"));
    let receipt = normalized(&output);
    assert!(receipt.contains(&format!("select it with --connection {id}")));
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ego.home_path().join("auth.json")).unwrap()).unwrap();
    assert_eq!(
        stored[format!("connection:{id}")]["access"].as_str(),
        Some("sk-ant-oatTestOnly0405")
    );
}

// Catches: interrupting an active device poll writes a token or strands the terminal owner.
#[test]
fn device_poll_interrupt_preserves_existing_credentials_and_the_next_turn() {
    let stub = Stub::start(Plan {
        completions: vec![Reply::Sse(text("DEVICE-CANCEL-TURN"))],
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
    ego.file("AGENTS.md", "# OAuth cancellation fixture\n");
    let previous = "{\"github-copilot\":{\"type\":\"o_auth\",\"access\":\"fixture-saved\",\"expires\":4102444800000}}\n";
    let path = ego.home_file("auth.json", previous);
    let (session, mut input, reader, mut output) = login_session(&ego, true);
    let start = output.len();
    input.write_all(b"/login github-copilot\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "fixture device poll pending");
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "login cancelled");
    until_prompt_from(&reader, &mut output, start);
    assert!(std::fs::read(&path).unwrap() == previous.as_bytes());
    let start = output.len();
    input.write_all(b"continue after cancellation\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "DEVICE-CANCEL-TURN");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(stub.generations().len(), 1);
    assert!(std::fs::read(path).unwrap() == previous.as_bytes());
}

fn login_session(
    ego: &Ego,
    fixture: bool,
) -> (PtySession, PtyWriter, mpsc::Receiver<Vec<u8>>, Vec<u8>) {
    let command = if fixture {
        ego.pty_command_with_program(env!("CARGO_BIN_EXE_ego-oauth-pty-fixture"))
    } else {
        ego.pty_command()
    };
    let (session, mut input, reader) = PtySession::start_command(command, 30, 200, None);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    (session, input, reader, output)
}

fn browser_lock() -> fd_lock::RwLock<std::fs::File> {
    fd_lock::RwLock::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(std::env::temp_dir().join("ego-provider-oauth-pty.lock"))
            .unwrap(),
    )
}
