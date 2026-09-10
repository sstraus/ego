//! The listener, driven over a real socket.
//!
//! The unit tests in `auth`, `bind` and `card` each pin one decision. This
//! drives the three of them together the way a caller does: a TCP connection, a
//! request written as bytes, an answer parsed as bytes. What it exists to prove
//! is the *order* — that the credential is checked before anything else about
//! the request is looked at — which is a property of the composed listener and
//! of nothing smaller.

use std::path::PathBuf;
use std::time::Duration;

use ego_a2a::auth::Credential;
use ego_a2a::serve::{WELL_KNOWN_AGENT_CARD_PATH, serve_on};
use ego_core::endpoint::{Admitted, PublicBind, Requested, admit};
use ego_listen::{ConnectionGate, ConnectionLimits};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::oneshot;

const TOKEN: &str = "listener-test-token";

/// One raw HTTP/1.1 answer: the status line, and the body.
struct Answer {
    status: u16,
    headers: String,
    body: String,
}

impl Answer {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("the body is not JSON ({error}): {:?}", self.body))
    }
}

struct Running {
    address: String,
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Running {
    async fn shutdown(self) {
        self.stop.send(()).expect("the listener is still running");
        tokio::time::timeout(Duration::from_secs(1), self.task)
            .await
            .expect("the listener shuts down while idle")
            .expect("the listener task joins")
            .expect("the listener shuts down cleanly");
    }
}

/// Serve `admitted`, and report the endpoint it actually got.
async fn serving(admitted: Admitted, credential: Option<Credential>) -> Running {
    serving_with_limits(admitted, credential, ConnectionLimits::default()).await
}

async fn serving_with_limits(
    admitted: Admitted,
    credential: Option<Credential>,
    limits: ConnectionLimits,
) -> Running {
    let bound = ego_listen::bind(&admitted)
        .await
        .expect("the endpoint binds");
    let address = bound.advertised().expect("a bound endpoint names itself");
    let gate = ConnectionGate::new(limits);
    let (stop, shutdown) = oneshot::channel();
    let task = tokio::spawn(async move {
        serve_on(bound, &admitted, credential, gate, async move {
            let _ = shutdown.await;
        })
        .await
    });
    Running {
        address,
        stop,
        task,
    }
}

/// A listener on an OS-chosen port, and the address it actually got.
async fn listening() -> Running {
    let admitted = admit(
        Requested::Tcp("127.0.0.1:0".parse().expect("an address")),
        PublicBind::Refused,
    )
    .expect("loopback is admitted");
    let credential =
        Credential::from_env(|_| Some(TOKEN.to_string())).expect("a non-empty token is admissible");
    serving(admitted, Some(credential)).await
}

/// The same listener on a local endpoint, where the operating system is the
/// authentication and no credential exists at all.
///
/// Under `~/Gits` deliberately: `CLAUDE.md` keeps test paths out of the per-user
/// temp dir on this machine.
async fn listening_locally(name: &str) -> (PathBuf, Running) {
    let mut path = PathBuf::from(std::env::var("HOME").expect("HOME is set"));
    path.push("Gits/.tmp/ego-a2a-tests");
    path.push(name);
    path.push("a2a.sock");
    let admitted = admit(
        Requested::Local {
            name: path.display().to_string(),
        },
        PublicBind::Refused,
    )
    .expect("a local endpoint is admitted");
    let running = serving(admitted, None).await;
    (path, running)
}

/// Write one request verbatim and read the whole answer.
///
/// Hand-written rather than through a client crate: the point of this suite is
/// what goes over the wire, and a client that normalises a request on the way
/// out would be testing the client.
async fn exchange(address: &str, request: &str) -> Answer {
    let stream = TcpStream::connect(address)
        .await
        .expect("the listener accepts");
    exchange_on(stream, request).await
}

/// The same, on a connection the caller already opened — which is how the local
/// endpoint is driven, since it has no address to dial.
async fn exchange_on(mut stream: impl AsyncRead + AsyncWrite + Unpin, request: &str) -> Answer {
    stream
        .write_all(request.as_bytes())
        .await
        .expect("the listener reads");
    stream.flush().await.expect("the listener reads");

    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut raw))
        .await
        .expect("the listener answered within five seconds")
        .expect("the answer is readable");
    let raw = String::from_utf8(raw).expect("the answer is UTF-8");

    let (head, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("the answer has no header/body boundary: {raw:?}"));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("the answer has no status: {head:?}"));
    Answer {
        status,
        headers: head.to_string(),
        body: body.to_string(),
    }
}

/// Read one response without closing the keep-alive connection.
async fn exchange_one(stream: &mut TcpStream, request: &str) -> Answer {
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();

    read_response_one(stream).await
}

async fn read_response_one(stream: &mut TcpStream) -> Answer {
    let mut raw = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await.unwrap();
        raw.extend_from_slice(&byte);
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let separator = raw.len() - 4;
    let head = String::from_utf8(raw[..separator].to_vec()).unwrap();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap();
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then_some(value.trim())
        })
        .and_then(|value| value.parse::<usize>().ok())
        .expect("a keep-alive response has a content length");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    Answer {
        status,
        headers: head,
        body: String::from_utf8(body).unwrap(),
    }
}

fn get(path: &str, authorization: Option<&str>) -> String {
    request("GET", path, authorization, None)
}

fn request(method: &str, path: &str, authorization: Option<&str>, body: Option<&str>) -> String {
    let mut head = format!("{method} {path} HTTP/1.1\r\nhost: localhost\r\nconnection: close\r\n");
    if let Some(authorization) = authorization {
        head.push_str(&format!("authorization: {authorization}\r\n"));
    }
    match body {
        Some(body) => format!(
            "{head}content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
        None => format!("{head}\r\n"),
    }
}

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

#[tokio::test]
async fn an_authenticated_caller_discovers_the_card_at_the_well_known_path() {
    let running = listening().await;
    let advertised = running.address.clone();
    let answer = exchange(
        &running.address,
        &get(WELL_KNOWN_AGENT_CARD_PATH, Some(&bearer())),
    )
    .await;
    running.shutdown().await;

    assert_eq!(answer.status, 200, "{}", answer.body);
    let card = answer.json();
    assert_eq!(card["name"], "ego");
    assert_eq!(card["protocolVersion"], serde_json::Value::Null);
    assert_eq!(
        card["supportedInterfaces"][0]["protocolVersion"],
        ego_a2a::PROTOCOL_VERSION
    );
    // The card must advertise the address this socket actually got, not the
    // one that was asked for: the test asked for port 0.
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        format!("http://{advertised}/")
    );
    assert_eq!(card["capabilities"]["streaming"], false);
}

/// The refusal must come before routing, so an anonymous caller cannot use the
/// listener to learn what it serves.
#[tokio::test]
async fn an_unauthenticated_caller_learns_nothing_at_all() {
    let running = listening().await;

    let cases = [
        ("no header", get(WELL_KNOWN_AGENT_CARD_PATH, None)),
        (
            "wrong token",
            get(WELL_KNOWN_AGENT_CARD_PATH, Some("Bearer not-the-token")),
        ),
        (
            "wrong scheme",
            get(WELL_KNOWN_AGENT_CARD_PATH, Some(&format!("Basic {TOKEN}"))),
        ),
        ("empty header", get(WELL_KNOWN_AGENT_CARD_PATH, Some(""))),
        (
            "a valid envelope on the RPC route",
            request(
                "POST",
                "/",
                None,
                Some(r#"{"jsonrpc":"2.0","id":1,"method":"GetTask","params":{}}"#),
            ),
        ),
        ("a path that does not exist", get("/nope", None)),
    ];

    for (name, raw) in cases {
        let answer = exchange(&running.address, &raw).await;
        assert_eq!(answer.status, 401, "{name}: {}", answer.body);
        assert!(
            answer.body.is_empty(),
            "{name}: the refusal says something: {:?}",
            answer.body
        );
        assert!(
            answer
                .headers
                .to_ascii_lowercase()
                .contains("www-authenticate: bearer"),
            "{name}: no challenge: {:?}",
            answer.headers
        );
        // A 404 and a 401 must not be distinguishable by anything but the code
        // the anonymous caller is entitled to, or the listener is a directory.
        assert!(
            !answer.body.contains("ego"),
            "{name}: the refusal names the agent: {:?}",
            answer.body
        );
    }
    running.shutdown().await;
}

/// An authenticated caller gets the honest answer for this milestone: the
/// method is real, and this build has no route for it.
#[tokio::test]
async fn every_a2a_method_is_recognised_and_refused_as_unserved() {
    let running = listening().await;

    for method in [
        "SendMessage",
        "SendStreamingMessage",
        "GetTask",
        "ListTasks",
        "CancelTask",
        "SubscribeToTask",
        "GetExtendedAgentCard",
    ] {
        let body = format!(r#"{{"jsonrpc":"2.0","id":7,"method":"{method}","params":{{}}}}"#);
        let answer = exchange(
            &running.address,
            &request("POST", "/", Some(&bearer()), Some(&body)),
        )
        .await;
        assert_eq!(answer.status, 200, "{method}: {}", answer.body);
        let rpc = answer.json();
        assert_eq!(rpc["id"], 7, "{method}");
        assert_eq!(rpc["error"]["code"], -32601, "{method}");
        let message = rpc["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(method), "{method}: {message}");
        assert!(
            message.contains("does not serve yet"),
            "{method}: an unserved method reads as unknown: {message}"
        );
    }
    running.shutdown().await;
}

/// A method A2A does not define reads differently from one it does, so a caller
/// can tell a typo from a version gap.
#[tokio::test]
async fn a_method_a2a_does_not_define_reads_as_no_such_method() {
    let running = listening().await;
    let answer = exchange(
        &running.address,
        &request(
            "POST",
            "/",
            Some(&bearer()),
            Some(r#"{"jsonrpc":"2.0","id":8,"method":"message/send","params":{}}"#),
        ),
    )
    .await;
    running.shutdown().await;

    let rpc = answer.json();
    assert_eq!(rpc["error"]["code"], -32601);
    let message = rpc["error"]["message"].as_str().unwrap_or_default();
    // `message/send` is the v0.2/v0.3 spelling of `SendMessage`. It is not an
    // A2A v1.0 method, and answering it as one would hide the version gap.
    assert!(
        message.contains("is not an A2A method"),
        "the v0.3 spelling was accepted as v1.0: {message}"
    );
}

#[tokio::test]
async fn a_body_that_is_not_a_jsonrpc_envelope_is_a_parse_error() {
    let running = listening().await;
    let answer = exchange(
        &running.address,
        &request("POST", "/", Some(&bearer()), Some("{not json")),
    )
    .await;
    running.shutdown().await;

    assert_eq!(answer.json()["error"]["code"], -32700);
}

/// The unification, end to end: the same routes, the same card, over the
/// endpoint kind the operating system authenticates instead of a credential.
#[tokio::test]
async fn a_local_endpoint_serves_its_owner_with_no_credential_anywhere() {
    let (path, running) = listening_locally("card").await;
    let stream = UnixStream::connect(&path)
        .await
        .expect("the owner reaches its own endpoint");
    let answer = exchange_on(stream, &get(WELL_KNOWN_AGENT_CARD_PATH, None)).await;
    running.shutdown().await;

    assert_eq!(answer.status, 200, "{}", answer.body);
    let card = answer.json();
    assert_eq!(card["name"], "ego");
    // The card must not send its reader to configure a secret this endpoint
    // will never read.
    assert!(
        card.get("securitySchemes")
            .is_none_or(serde_json::Value::is_null),
        "a local endpoint advertised a security scheme: {card}"
    );
}

/// A bearer token is not a second way in on an endpoint the transport already
/// answered for, and presenting a wrong one is not a way to be refused.
#[tokio::test]
async fn a_local_endpoint_ignores_the_authorization_header_entirely() {
    let (path, running) = listening_locally("header").await;
    for authorization in ["Bearer not-the-token", "Basic whatever", ""] {
        let stream = UnixStream::connect(&path)
            .await
            .expect("the owner reaches its own endpoint");
        let answer = exchange_on(
            stream,
            &get(WELL_KNOWN_AGENT_CARD_PATH, Some(authorization)),
        )
        .await;
        assert_eq!(
            answer.status, 200,
            "{authorization:?} changed the answer on an endpoint the OS authenticated: {}",
            answer.body
        );
    }
    running.shutdown().await;
}

/// The refusal is at startup, not at the first request: an endpoint that cannot
/// judge a caller must never accept one.
#[tokio::test]
async fn a_loopback_endpoint_refuses_to_start_without_a_credential() {
    let admitted = admit(
        Requested::Tcp("127.0.0.1:0".parse().expect("an address")),
        PublicBind::Refused,
    )
    .expect("loopback is admitted");
    let bound = ego_listen::bind(&admitted)
        .await
        .expect("the endpoint binds");
    let gate = ConnectionGate::new(ConnectionLimits::default());
    let error = serve_on(bound, &admitted, None, gate, std::future::pending::<()>())
        .await
        .expect_err("a loopback endpoint served with no credential");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    let rendered = error.to_string();
    assert!(rendered.contains("EGO_A2A_TOKEN"), "{rendered}");
    assert!(rendered.contains("local endpoint"), "{rendered}");
}

#[tokio::test]
async fn an_authenticated_caller_still_gets_404_for_a_path_that_does_not_exist() {
    let running = listening().await;
    let answer = exchange(&running.address, &get("/nope", Some(&bearer()))).await;
    running.shutdown().await;
    assert_eq!(answer.status, 404);
}

#[tokio::test]
async fn an_idle_listener_shuts_down_without_aborting_its_task() {
    let running = listening().await;
    running.shutdown().await;
}

#[tokio::test]
async fn a_keepalive_connection_holds_its_gate_permit_until_disconnect() {
    let admitted = admit(
        Requested::Tcp("127.0.0.1:0".parse().unwrap()),
        PublicBind::Refused,
    )
    .unwrap();
    let running = serving_with_limits(
        admitted,
        Some(Credential::from_env(|_| Some(TOKEN.to_owned())).unwrap()),
        ConnectionLimits::new(1, Duration::from_secs(60), 2 * 1024 * 1024).unwrap(),
    )
    .await;
    let keepalive = request("GET", WELL_KNOWN_AGENT_CARD_PATH, Some(&bearer()), None)
        .replace("connection: close", "connection: keep-alive");
    let mut first = TcpStream::connect(&running.address).await.unwrap();
    let answer = exchange_one(&mut first, &keepalive).await;
    assert_eq!(answer.status, 200);

    let mut second = TcpStream::connect(&running.address).await.unwrap();
    let second_request = request("GET", WELL_KNOWN_AGENT_CARD_PATH, Some(&bearer()), None);
    second.write_all(second_request.as_bytes()).await.unwrap();
    second.flush().await.unwrap();
    let blocked =
        tokio::time::timeout(Duration::from_millis(100), read_response_one(&mut second)).await;
    assert!(
        blocked.is_err(),
        "the second keep-alive occupied a permit early"
    );

    drop(first);
    let released = tokio::time::timeout(Duration::from_secs(1), async {
        let mut raw = Vec::new();
        second.read_to_end(&mut raw).await.unwrap();
        raw
    })
    .await
    .expect("the second connection proceeds after the first disconnects");
    assert!(String::from_utf8_lossy(&released).starts_with("HTTP/1.1 200"));
    running.shutdown().await;
}

#[tokio::test]
async fn a2a_rejects_bodies_over_one_mib_even_with_a_larger_connection_budget() {
    let admitted = admit(
        Requested::Tcp("127.0.0.1:0".parse().unwrap()),
        PublicBind::Refused,
    )
    .unwrap();
    let running = serving_with_limits(
        admitted,
        Some(Credential::from_env(|_| Some(TOKEN.to_owned())).unwrap()),
        ConnectionLimits::new(1, Duration::from_secs(60), 2 * 1024 * 1024).unwrap(),
    )
    .await;
    let body = "x".repeat(1024 * 1024 + 1);
    let answer = exchange(
        &running.address,
        &request("POST", "/", Some(&bearer()), Some(&body)),
    )
    .await;
    running.shutdown().await;
    assert_eq!(answer.status, 413, "{}", answer.body);
}
