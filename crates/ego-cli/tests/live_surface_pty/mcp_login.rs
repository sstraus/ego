use super::*;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};

// Catches: OAuth stores the credential but the running REPL still needs a restart
// before its MCP tools are admitted and offered to the next generation.
#[test]
fn mcp_login_dcr_reconnects_and_lists_tools_without_restart() {
    login_reconnect_contract(false);
}

// Catches: an issuer advertising more than one strategy is silently registered
// instead of asking the user before the first registration POST.
#[test]
fn mcp_login_multiple_strategies_waits_for_inline_choice() {
    login_reconnect_contract(true);
}

fn login_reconnect_contract(cimd: bool) {
    let oauth = OAuthServer::start(cimd, false);
    let provider = Stub::start(Plan {
        completions: vec![Reply::Sse(text("MCP-LOGIN-TOOLS-READY"))],
        ..Plan::default()
    });
    let (ego, config_path, config) = oauth_terminal(&provider, &oauth);
    let (session, mut input, reader) = start_oauth_pty(&ego);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/mcp login remote\r").unwrap();
    input.flush().unwrap();
    if cimd {
        until_normalized(&reader, &mut output, start, "Choose how to identify");
        assert!(
            !oauth
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(path, _)| path == "/register")
        );
        input.write_all(b"\r").unwrap();
        input.flush().unwrap();
    }
    let deadline = Instant::now() + PROGRESS_DEADLINE;
    loop {
        let receipt = normalized(&output[start..]);
        if receipt.contains("remote admitted") || receipt.contains("MCP control failed:") {
            break;
        }
        output.extend(
            reader
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap_or_else(|error| panic!("login receipt missing: {error}; {receipt}")),
        );
    }
    let receipt = normalized(&output[start..]);
    let connected = receipt.contains("remote admitted");
    until_prompt_from(&reader, &mut output, start);
    if connected {
        let start = output.len();
        input.write_all(b"show available tools\r").unwrap();
        input.flush().unwrap();
        until_normalized(&reader, &mut output, start, "MCP-LOGIN-TOOLS-READY");
        until_prompt_from(&reader, &mut output, start);
    }
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert!(
        connected,
        "login did not reconnect in the same REPL: {receipt}"
    );
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), config);
    let generations = provider.generations();
    assert_eq!(generations.len(), 1);
    assert!(
        generations[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "fixture_lookup")
    );
    let requests = oauth.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(path, _)| path == "/register")
            .count(),
        1
    );
    assert_eq!(
        requests.iter().filter(|(path, _)| path == "/token").count(),
        1
    );
    assert!(requests.iter().any(|(path, request)| {
        path == "/mcp"
            && request
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-access")
    }));
}

// Catches: a denied selected strategy is retried with another registration,
// publishes credentials, or enables a session-disabled server despite failure.
#[test]
fn mcp_login_denial_preserves_session_state_without_fallback() {
    let oauth = OAuthServer::start(false, true);
    let provider = Stub::start(Plan::default());
    let (ego, config_path, config) = oauth_terminal(&provider, &oauth);
    let (session, mut input, reader) = start_oauth_pty(&ego);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/mcp toggle remote\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "remote off (session)");
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/mcp login remote\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "authorization was denied");
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/mcp\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "off (session)");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), config);
    assert!(provider.generations().is_empty());
    let requests = oauth.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|(path, _)| path == "/register")
            .count(),
        1
    );
    assert!(
        !requests
            .iter()
            .any(|(path, _)| path == "/token" || path == "/mcp")
    );
    assert!(
        !std::fs::read_dir(ego.home_path().join("mcp"))
            .unwrap()
            .any(|entry| {
                let path = entry.unwrap().path();
                path.extension()
                    .is_some_and(|extension| extension == "json")
                    && path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("oauth-")
            })
    );
}

fn oauth_terminal(provider: &Stub, oauth: &OAuthServer) -> (Ego, std::path::PathBuf, String) {
    let ego = Ego::against(provider).conversing().args([
        "--model",
        "ollama/qwen3",
        "--sandbox",
        "off",
        "--mode",
        "yolo",
    ]);
    ego.file("AGENTS.md", "# MCP OAuth PTY fixture\n");
    let config = format!(
        "[mcp.servers.remote]\ntransport='http'\nurl='{}/mcp'\nauth='oauth'\nallow_insecure_loopback=true\n",
        oauth.origin,
    );
    let config_path = ego.home_file("config.toml", &config);
    let browser = ego.file("browser.py", "import sys, urllib.request\nwith urllib.request.urlopen(sys.argv[1], timeout=5) as response:\n    assert response.status == 200\n");
    let launcher = format!(
        "#!/bin/sh\nexec /usr/bin/python3 {} \"$1\"\n",
        shell_words::quote(browser.to_str().unwrap())
    );
    for name in ["open", "xdg-open"] {
        let path = ego.file(name, &launcher);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let path = format!("{}:/usr/bin:/bin", ego.workspace_path().display());
    let ego = ego.env("PATH", &path);
    (ego, config_path, config)
}

fn start_oauth_pty(ego: &Ego) -> (PtySession, PtyWriter, mpsc::Receiver<Vec<u8>>) {
    PtySession::start_command(
        ego.pty_command_with_program(env!("CARGO_BIN_EXE_ego-oauth-pty-fixture")),
        30,
        140,
        None,
    )
}

// Catches: waiting for the inline strategy choice blocks the runtime, so
// Ctrl-C cannot cancel OAuth and restore the ordinary composer.
#[test]
fn mcp_login_cancelled_inline_choice_restores_prompt_without_registration() {
    let oauth = OAuthServer::start(true, false);
    let provider = Stub::start(Plan::default());
    let (ego, config_path, config) = oauth_terminal(&provider, &oauth);
    let (session, mut input, reader) = start_oauth_pty(&ego);
    let mut output = Vec::new();
    until(&reader, &mut output, b"Trust project files");
    let start = output.len();
    input.write_all(b"yes\r").unwrap();
    input.flush().unwrap();
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/mcp login remote\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "Choose how to identify");
    input.write_all(b"\x03").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "MCP OAuth login cancelled");
    until_prompt_from(&reader, &mut output, start);
    let start = output.len();
    input.write_all(b"/mcp\r").unwrap();
    input.flush().unwrap();
    until_normalized(&reader, &mut output, start, "/mcp toggle NAME");
    until_prompt_from(&reader, &mut output, start);
    input.write_all(b"/quit\r").unwrap();
    input.flush().unwrap();
    assert!(session.quit(input).success());
    assert_eq!(std::fs::read_to_string(config_path).unwrap(), config);
    assert!(provider.generations().is_empty());
    assert!(
        !oauth
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|(path, _)| path == "/register" || path == "/token")
    );
}

struct OAuthServer {
    origin: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl OAuthServer {
    fn start(cimd: bool, denied: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server_origin = origin.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("OAuth accept: {error}"),
                };
                let (path, headers, body) = oauth_request(&mut socket);
                recorded.lock().unwrap().push((path.clone(), headers));
                let payload = if path.contains("oauth-protected-resource") {
                    serde_json::json!({"resource":format!("{server_origin}/mcp"), "authorization_servers":[server_origin]})
                } else if path.contains("oauth-authorization-server") {
                    serde_json::json!({"issuer":server_origin,"authorization_endpoint":format!("{server_origin}/authorize"),"token_endpoint":format!("{server_origin}/token"),"registration_endpoint":format!("{server_origin}/register"),"token_endpoint_auth_methods_supported":["none"],"client_id_metadata_document_supported":cimd})
                } else if path == "/register" {
                    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(request["token_endpoint_auth_method"], "none");
                    serde_json::json!({"client_id":"fixture-public", "token_endpoint_auth_method":"none", "redirect_uris":request["redirect_uris"]})
                } else if path.starts_with("/authorize?") {
                    let url: url::Url = format!("{server_origin}{path}").parse().unwrap();
                    let fields = url
                        .query_pairs()
                        .into_owned()
                        .collect::<std::collections::HashMap<_, _>>();
                    assert_eq!(fields["client_id"], "fixture-public");
                    assert_eq!(fields["code_challenge_method"], "S256");
                    let mut callback: url::Url = fields["redirect_uri"].parse().unwrap();
                    callback
                        .query_pairs_mut()
                        .append_pair(
                            if denied { "error" } else { "code" },
                            if denied {
                                "access_denied"
                            } else {
                                "fixture-code"
                            },
                        )
                        .append_pair("state", &fields["state"]);
                    write!(socket, "HTTP/1.1 302 Found\r\nLocation: {callback}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                    continue;
                } else if path == "/token" {
                    let fields = url::form_urlencoded::parse(&body)
                        .into_owned()
                        .collect::<std::collections::HashMap<_, _>>();
                    assert_eq!(fields["code"], "fixture-code");
                    assert_eq!(fields["client_id"], "fixture-public");
                    assert!(fields["code_verifier"].len() >= 43);
                    serde_json::json!({"access_token":"fixture-access", "refresh_token":"fixture-refresh", "token_type":"Bearer", "expires_in":3600})
                } else if path == "/mcp" {
                    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let result = if request["method"] == "server/discover" {
                        serde_json::json!({"resultType":"complete","supportedVersions":["2026-07-28"],"capabilities":{"tools":{"listChanged":false}},"ttlMs":0,"cacheScope":"private","_meta":{"io.modelcontextprotocol/serverInfo":{"name":"remote","version":"1"}}})
                    } else {
                        serde_json::json!({"resultType":"complete","tools":[{"name":"fixture_lookup","description":"Look up a fixture","inputSchema":{"type":"object","properties":{}}}],"nextCursor":null,"ttlMs":0,"cacheScope":"private"})
                    };
                    serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                } else {
                    panic!("unexpected OAuth fixture path {path}");
                };
                let body = payload.to_string();
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            origin,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for OAuthServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.thread.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

fn oauth_request(socket: &mut TcpStream) -> (String, String, Vec<u8>) {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 4096];
        let size = socket.read(&mut chunk).unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&chunk[..size]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < end + length {
        let mut chunk = [0; 4096];
        let size = socket.read(&mut chunk).unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&chunk[..size]);
    }
    let path = headers.split_whitespace().nth(1).unwrap().to_owned();
    (path, headers, bytes[end..end + length].to_vec())
}
