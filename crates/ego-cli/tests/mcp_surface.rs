//! contract for MCP protocol-facing CLI composition.
//!
//! These assertions are isolated: they exercise
//! the Login/Reload actions and `--mcp` attachment contract directly.

use std::sync::Mutex;

use ego_cli::cli::{Invocation, McpAction, McpSelector, parse};
use ego_cli::output::{Diagnostics, TextOutput, TextSink};
use ego_cli::render::Capabilities;
use ego_cli::run::{Ended, report};
use ego_core::event::id::SessionId;
use ego_core::public::{PUBLIC_EVENT_VERSION, PublicEvent};
use ego_core::run::RunTermination;
use ego_core::sink::EventSink;
use ego_core::state::SessionState;

mod support;

#[derive(Default)]
struct CapturedText {
    output: Mutex<String>,
    diagnostics: Mutex<Vec<String>>,
}

impl Diagnostics for CapturedText {
    fn note(&self, message: &str) {
        self.diagnostics
            .lock()
            .expect("diagnostic capture")
            .push(message.to_owned());
    }
}

impl TextOutput for CapturedText {
    fn write(&self, text: &str) {
        self.output.lock().expect("text capture").push_str(text);
    }

    fn line(&self, text: &str) {
        let mut output = self.output.lock().expect("text capture");
        output.push_str(text);
        output.push('\n');
    }
}

#[test]
fn run_keeps_ordered_named_and_file_selectors() {
    let Invocation::Run(args) = parse(
        [
            "ego",
            "run",
            "--mcp",
            "remote",
            "--mcp",
            "@scope.json",
            "prompt",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
    .expect("valid run composition") else {
        panic!("expected run")
    };
    assert_eq!(
        args.mcp,
        vec![
            McpSelector::Named("remote".parse().expect("name")),
            McpSelector::File("scope.json".into())
        ]
    );
}

#[test]
fn final_management_grammar_composes_login_and_reload_as_typed_actions() {
    let login = parse(
        ["ego", "mcp", "login", "remote"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("login is a current MCP management action");
    assert!(
        matches!(login, Invocation::Mcp(args) if matches!(args.action, McpAction::Login { ref name, .. } if name.to_string() == "remote"))
    );

    let reload = parse(
        ["ego", "mcp", "reload"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("reload is a current MCP management action");
    assert!(
        matches!(reload, Invocation::Mcp(args) if matches!(args.action, McpAction::Reload { name: None }))
    );

    let reload_one = parse(
        ["ego", "mcp", "reload", "remote"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("named reload is a current MCP management action");
    assert!(
        matches!(reload_one, Invocation::Mcp(args) if matches!(args.action, McpAction::Reload { name: Some(ref name) } if name.to_string() == "remote"))
    );
}

#[test]
fn run_composes_current_mcp_selector() {
    let invocation = parse(
        ["ego", "run", "--mcp", "remote", "prompt"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("--mcp is a current run composition input");
    let Invocation::Run(args) = invocation else {
        panic!("expected run invocation")
    };
    assert_eq!(args.mcp.len(), 1);
    assert!(matches!(&args.mcp[0], McpSelector::Named(name) if name.to_string() == "remote"));
}

#[test]
fn an_empty_mcp_selection_attaches_no_servers() {
    let Invocation::Run(args) = parse(
        ["ego", "run", "prompt"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    )
    .expect("a run without MCP selectors is valid") else {
        panic!("expected run")
    };
    assert!(args.mcp.is_empty());
}

#[test]
fn malformed_mcp_selectors_fail_before_a_run_exists() {
    for selector in ["@", "", "bad name", "../scope.json"] {
        let mut argv = vec!["ego".to_owned(), "run".to_owned(), "--mcp".to_owned()];
        argv.push(selector.to_owned());
        argv.push("prompt".to_owned());
        assert!(parse(argv).is_err(), "selector {selector:?} was accepted");
    }
}

#[test]
fn repeated_selectors_preserve_argv_order_for_preflight_diagnostics() {
    let Invocation::Run(args) = parse(
        [
            "ego", "run", "--mcp", "zeta", "--mcp", "alpha", "--mcp", "zeta", "prompt",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
    )
    .expect("selector grammar is parsed before duplicate preflight") else {
        panic!("expected run")
    };
    assert_eq!(
        args.mcp,
        vec![
            McpSelector::Named("zeta".parse().expect("name")),
            McpSelector::Named("alpha".parse().expect("name")),
            McpSelector::Named("zeta".parse().expect("name")),
        ]
    );
}

#[test]
fn unknown_and_duplicate_named_selectors_fail_before_session_or_provider() {
    let places = support::Places::new();
    let stub = support::Stub::start(support::Plan::default());
    places.ego(&stub).home_file(
        "config.toml",
        "[mcp.servers.remote]\ntransport = 'http'\nurl = 'https://mcp.example.test/mcp'\nauth = 'none'\n",
    );

    for selectors in [
        vec!["missing".to_owned()],
        vec!["remote".to_owned(), "remote".to_owned()],
    ] {
        let mut ego = places.ego(&stub).args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
        ]);
        for selector in &selectors {
            ego = ego.arg("--mcp").arg(selector);
        }
        let finished = ego.arg("prompt").run();
        assert_ne!(
            finished.code,
            Some(0),
            "invalid selector set ran: {selectors:?}"
        );
        assert!(
            stub.requests().is_empty(),
            "provider was contacted before MCP preflight rejected {selectors:?}"
        );
    }
}

#[test]
fn strict_file_and_config_name_collision_fails_before_session_or_provider() {
    let places = support::Places::new();
    let stub = support::Stub::start(support::Plan::default());
    let ego = places.ego(&stub);
    ego.home_file(
        "config.toml",
        "[mcp.servers.remote]\ntransport = 'http'\nurl = 'https://mcp.example.test/mcp'\nauth = 'none'\n",
    );
    ego.file(
        "scope.json",
        r#"{
  "name": "remote",
  "transport": "http",
  "url": "https://mcp.example.test/other",
  "auth": "none"
}
"#,
    );

    let finished = ego
        .args([
            "--model",
            "ollama/qwen3",
            "--session-id",
            support::FIXTURE_SESSION_ID,
            "--mcp",
            "@scope.json",
            "prompt",
        ])
        .run();
    assert_ne!(
        finished.code,
        Some(0),
        "file/config name collision was accepted"
    );
    assert!(
        stub.requests().is_empty(),
        "provider was contacted before file/config collision was rejected"
    );
}

#[test]
fn strict_invocation_file_requires_one_explicit_name_and_known_fields() {
    for contents in [
        r#"{
  "transport": "http",
  "url": "https://mcp.example.test/mcp",
  "auth": "none"
}
"#,
        r#"{
  "name": "remote",
  "transport": "http",
  "url": "https://mcp.example.test/mcp",
  "auth": "none",
  "unexpected": true
}
"#,
    ] {
        let places = support::Places::new();
        let stub = support::Stub::start(support::Plan::default());
        let ego = places.ego(&stub);
        ego.file("scope.json", contents);
        let finished = ego
            .args([
                "--model",
                "ollama/qwen3",
                "--session-id",
                support::FIXTURE_SESSION_ID,
                "--mcp",
                "@scope.json",
                "prompt",
            ])
            .run();
        assert_ne!(finished.code, Some(0), "non-strict MCP file was accepted");
        assert!(
            stub.requests().is_empty(),
            "provider was contacted before strict MCP file rejection"
        );
    }
}

#[test]
fn one_sorted_sanitized_mcp_error_set_reaches_event_text_and_completion_json() {
    let errors = vec![
        "server `alpha` is unavailable".to_owned(),
        "server `zeta` requires authorization".to_owned(),
    ];
    let session = SessionId::new();
    let event = PublicEvent::SessionStarted {
        v: PUBLIC_EVENT_VERSION,
        session,
        capabilities: Vec::new(),
        mcp_errors: errors.clone(),
        hook_errors: Vec::new(),
        instructions: Vec::new(),
    };

    let text = CapturedText::default();
    let sink = TextSink::new(&text, false, Capabilities::plain());
    sink.publish(&event, &SessionState::default());
    let diagnostics = text.diagnostics.lock().expect("diagnostic capture").clone();
    assert_eq!(diagnostics.len(), 2, "text mode must report each MCP error");
    assert!(diagnostics[0].contains(&errors[0]));
    assert!(diagnostics[1].contains(&errors[1]));

    let event_json = serde_json::to_value(&event).expect("SessionStarted serializes");
    assert_eq!(event_json["mcp_errors"], serde_json::json!(errors));

    let ended = Ended {
        termination: RunTermination::Done,
        session,
        state: SessionState::default(),
        result: serde_json::Value::String("done".to_owned()),
        selected_session: None,
        mcp_errors: errors.clone(),
    };
    let completion = serde_json::to_value(report(&ended)).expect("completion report serializes");
    assert_eq!(completion["mcp_errors"], serde_json::json!(errors));

    let empty_event = PublicEvent::SessionStarted {
        v: PUBLIC_EVENT_VERSION,
        session,
        capabilities: Vec::new(),
        mcp_errors: Vec::new(),
        hook_errors: Vec::new(),
        instructions: Vec::new(),
    };
    assert!(
        !serde_json::to_value(empty_event)
            .expect("empty SessionStarted serializes")
            .as_object()
            .expect("event object")
            .contains_key("mcp_errors")
    );
    let empty_completion = Ended {
        termination: RunTermination::Done,
        session,
        state: SessionState::default(),
        result: serde_json::Value::String("done".to_owned()),
        selected_session: None,
        mcp_errors: Vec::new(),
    };
    assert!(
        !serde_json::to_value(report(&empty_completion))
            .expect("empty completion serializes")
            .as_object()
            .expect("completion object")
            .contains_key("mcp_errors")
    );
}

#[test]
fn login_rejects_an_unconfigured_server_without_contacting_a_provider() {
    let places = support::Places::new();
    let stub = support::Stub::start(support::Plan::default());
    let finished = places
        .ego(&stub)
        .subcommand("mcp")
        .args(["login", "remote"])
        .run();
    assert_ne!(
        finished.code,
        Some(0),
        "login ran without a configured resource"
    );
    assert!(
        finished.stderr.contains("not configured"),
        "login refusal does not identify missing configuration: {}",
        finished.stderr
    );
    assert!(stub.requests().is_empty(), "login contacted the provider");
}

#[test]
fn oauth_login_prints_copyable_url_and_reports_denial_without_touching_credentials() {
    use std::io::{BufRead, Read, Write};
    use std::net::{TcpListener, TcpStream};
    let places = support::Places::new();
    let stub = support::Stub::start(support::Plan::default());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    std::fs::write(places.home_path().join("config.toml"), format!("[mcp.servers.remote]\ntransport = \"http\"\nurl = \"{origin}/mcp\"\nauth = \"oauth\"\nallow_insecure_loopback = true\n")).unwrap();
    let server_origin = origin.clone();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                .unwrap();
            let mut request = [0u8; 8192];
            let size = socket.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..size]);
            let body = if request.contains("oauth-protected-resource") {
                serde_json::json!({"resource":format!("{server_origin}/mcp"),"authorization_servers":[server_origin]})
            } else {
                serde_json::json!({"issuer":server_origin,"authorization_endpoint":format!("{server_origin}/authorize"),"token_endpoint":format!("{server_origin}/token")})
            }.to_string();
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let mut running = places
        .ego(&stub)
        .subcommand("mcp")
        .args(["login", "remote", "--client-id", "operator-client"])
        .spawn();
    let mut stdout = running.stdout();
    let mut text = String::new();
    let authorization = loop {
        let mut line = String::new();
        assert_ne!(
            stdout.read_line(&mut line).unwrap(),
            0,
            "login ended before authorization URL: {text}"
        );
        text.push_str(&line);
        if let Some(url) = line.trim().strip_prefix("Authorize: ") {
            break url.parse::<url::Url>().unwrap();
        }
    };
    let fields: std::collections::HashMap<_, _> =
        authorization.query_pairs().into_owned().collect();
    assert_eq!(fields["client_id"], "operator-client");
    assert_eq!(fields["code_challenge_method"], "S256");
    let callback: url::Url = fields["redirect_uri"].parse().unwrap();
    let mut socket =
        TcpStream::connect((callback.host_str().unwrap(), callback.port().unwrap())).unwrap();
    write!(
        socket,
        "GET /callback?error=access_denied&state={} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        fields["state"]
    )
    .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    stdout.read_to_string(&mut text).unwrap();
    let finished = running.finish();
    assert_ne!(finished.code, Some(0));
    assert!(
        finished.stderr.contains("authorization was denied"),
        "{}",
        finished.stderr
    );
    assert!(!finished.stderr.contains(&fields["state"]));
    assert!(!text.contains("code_verifier"));
    assert!(stub.requests().is_empty());
    server.join().unwrap();
}
