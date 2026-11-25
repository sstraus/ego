use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ego_core::event::domain::CancelCause;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{
    CanonPath, Confinement, DnsResolution, GuardedHttp, HttpRequest, HttpRoute, HttpScheme,
    NetworkTarget, OneHopResponse, PreparedConnect, Resolver, RunConfinement, TransportError,
    TransportFailure,
};
use ego_core::turn::CancelToken;
use futures::{StreamExt, future::BoxFuture};

use super::ProviderProxy;
use crate::exec::HttpCallTimeout;
use crate::http::OneShotHttpTransport;

#[derive(Default)]
struct ProxyResolver(Mutex<Vec<NetworkTarget>>);
impl Resolver for ProxyResolver {
    fn resolve(
        &self,
        target: NetworkTarget,
        _: Instant,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        self.0.lock().unwrap().push(target.clone());
        // Neither name exists in system DNS. A direct provider dial cannot succeed.
        let ip = if target.host() == "proxy.test" {
            Ipv4Addr::LOCALHOST
        } else {
            Ipv4Addr::new(192, 0, 2, 123)
        };
        Box::pin(async move { Ok(DnsResolution::new(target, vec![IpAddr::V4(ip)]).unwrap()) })
    }
}

fn transport(
    resolver: Arc<ProxyResolver>,
    port: u16,
    offline: bool,
    cancel: CancelToken,
    ca: bool,
) -> Arc<OneShotHttpTransport> {
    let proxy: ProviderProxy = format!("http://proxy.test:{port}").parse().unwrap();
    let proxy = if ca {
        proxy
            .with_ca_pem(include_bytes!("fixtures/ca.pem").to_vec())
            .unwrap()
    } else {
        proxy
    };
    Arc::new(
        OneShotHttpTransport::new(
            resolver,
            cancel,
            HttpCallTimeout::default(),
            None,
            RunConfinement::new(
                Confinement::Off,
                offline,
                CanonPath::new(env!("CARGO_MANIFEST_DIR")),
            )
            .unwrap(),
        )
        .with_proxy(proxy),
    )
}

async fn execute(
    transport: Arc<OneShotHttpTransport>,
    deadline: Instant,
) -> Result<OneHopResponse, TransportError> {
    let target = NetworkTarget::new("provider.test", 443).unwrap();
    let route = HttpRoute::new(HttpScheme::Https, target.clone()).unwrap();
    let request = HttpRequest::get("/v1/test").unwrap();
    GuardedHttp::new(transport.clone(), transport)
        .execute_admitted(
            route,
            PreparedConnect::new(target, &request),
            request,
            deadline,
        )
        .await
}

fn read_headers(stream: &mut impl Read) -> std::io::Result<String> {
    let mut result = Vec::new();
    while !result.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        result.push(byte[0]);
        if result.len() > 16384 {
            return Err(std::io::Error::other("oversized test headers"));
        }
    }
    Ok(String::from_utf8(result).unwrap())
}

fn serve_proxy(
    listener: TcpListener,
    response: &'static [u8],
) -> std::thread::JoinHandle<Result<(String, String, String), String>> {
    std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("proxy did not receive connection: {error}"),
            }
        };
        socket.set_nonblocking(false).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let connect = read_headers(&mut socket).unwrap();
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .unwrap();
        let cert =
            rustls::pki_types::CertificateDer::from(include_bytes!("fixtures/server.der").to_vec());
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(
            include_bytes!("fixtures/server-key.der").to_vec(),
        );
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert], key.into())
            .unwrap();
        let session = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut tls = rustls::StreamOwned::new(session, socket);
        let request = read_headers(&mut tls).map_err(|error| error.to_string())?;
        let sni = tls.conn.server_name().unwrap().to_owned();
        tls.write_all(response).unwrap();
        tls.flush().unwrap();
        Ok((connect, sni, request))
    })
}

// Catches: ambient/direct routing, unpinned proxy DNS, changed Host/SNI, or ignored scoped CA.
#[tokio::test]
async fn explicit_proxy_preserves_provider_tls_authority_and_response_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = serve_proxy(
        listener,
        b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nproxy stream",
    );
    let resolver = Arc::new(ProxyResolver::default());
    let result = execute(
        transport(resolver.clone(), port, false, CancelToken::new(), true),
        Instant::now() + Duration::from_secs(5),
    )
    .await;
    let observed = server.join();
    assert!(
        result.is_ok(),
        "provider TLS failed: {}",
        result.as_ref().err().unwrap()
    );
    let (connect, sni, request) = observed.unwrap().unwrap();
    let OneHopResponse::Response {
        status, mut body, ..
    } = result.unwrap()
    else {
        panic!("expected response")
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend(chunk.unwrap());
    }
    assert_eq!(status, 200);
    assert_eq!(bytes, b"proxy stream");
    assert!(
        connect.starts_with("CONNECT provider.test:443 HTTP/1.1\r\n"),
        "{connect}"
    );
    assert_eq!(sni, "provider.test");
    assert!(
        request.starts_with("GET /v1/test HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("\r\nhost: provider.test\r\n"),
        "{request}"
    );
    let names: Vec<_> = resolver
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|target| target.host().to_owned())
        .collect();
    assert_eq!(names, ["provider.test", "proxy.test"]);
}

// Catches: reqwest redirect following outside the caller's admitted next hop.
#[tokio::test]
async fn explicit_proxy_returns_redirect_without_following_it() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = serve_proxy(listener, b"HTTP/1.1 302 Found\r\nLocation: https://unadmitted.test/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    let result = execute(
        transport(Arc::default(), port, false, CancelToken::new(), true),
        Instant::now() + Duration::from_secs(5),
    )
    .await;
    server.join().unwrap().unwrap();
    assert!(
        matches!(result.unwrap(), OneHopResponse::Redirect { status: 302, location, .. } if location == b"https://unadmitted.test/secret")
    );
}

// Catches: a loopback proxy bypassing offline, expired budget, or cancellation guards.
#[tokio::test]
async fn explicit_proxy_refuses_before_socket_for_offline_deadline_and_cancel() {
    for case in ["offline", "deadline", "cancel"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let cancel = CancelToken::new();
        if case == "cancel" {
            cancel.cancel(CancelCause::User);
        }
        let deadline = if case == "deadline" {
            Instant::now()
        } else {
            Instant::now() + Duration::from_secs(5)
        };
        let error = execute(
            transport(
                Arc::default(),
                listener.local_addr().unwrap().port(),
                case == "offline",
                cancel,
                true,
            ),
            deadline,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            error.delivery(),
            DeliveryCertainty::DefinitelyUnsent,
            "{case}"
        );
        assert!(
            matches!(
                (case, error.failure()),
                ("offline", TransportFailure::Offline)
                    | ("deadline", TransportFailure::TimedOut)
                    | ("cancel", TransportFailure::Cancelled)
            ),
            "{case}: {error}"
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

// Catches: retrying a failed CONNECT directly or classifying an unsent provider request as delivered.
#[tokio::test]
async fn explicit_proxy_failed_connect_is_unsent_without_direct_fallback() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let connect = read_headers(&mut socket).unwrap();
        socket
            .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        connect
    });
    let error = execute(
        transport(Arc::default(), port, false, CancelToken::new(), false),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .err()
    .unwrap();
    assert!(
        server
            .join()
            .unwrap()
            .starts_with("CONNECT provider.test:443 ")
    );
    assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    assert!(
        matches!(error.failure(), TransportFailure::Connect(_)),
        "{error}"
    );
}

// Catches: silently sending WebSocket traffic directly when the user selected a proxy.
#[tokio::test]
async fn explicit_proxy_websocket_is_refused_without_proxy_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let transport = transport(
        Arc::default(),
        listener.local_addr().unwrap().port(),
        false,
        CancelToken::new(),
        true,
    );
    let target = NetworkTarget::new("provider.test", 443).unwrap();
    let route = HttpRoute::new(HttpScheme::Https, target.clone()).unwrap();
    let request = HttpRequest::get("/v1/test").unwrap();
    let error = GuardedHttp::new(transport.clone(), transport)
        .open_websocket_admitted(
            route,
            PreparedConnect::new(target, &request),
            request,
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    assert!(
        matches!(error.failure(), TransportFailure::InvalidRequest(message) if message.contains("WebSocket"))
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

// Catches: NO_PROXY bypassing an explicit proxy or HTTPS_PROXY overriding its pinned socket.
#[test]
fn explicit_proxy_ignores_ambient_proxy_and_bypass_variables() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "http::proxy::tests::explicit_proxy_preserves_provider_tls_authority_and_response_bytes", "--nocapture"])
        .env("HTTP_PROXY", "http://127.0.0.1:1")
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .env("ALL_PROXY", "socks5://127.0.0.1:1")
        .env("NO_PROXY", "*")
        .env("http_proxy", "http://127.0.0.1:1")
        .env("https_proxy", "http://127.0.0.1:1")
        .env("all_proxy", "socks5://127.0.0.1:1")
        .env("no_proxy", "*")
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

// Catches: disabling certificate verification instead of adding the explicitly selected CA.
#[tokio::test]
async fn explicit_proxy_without_ca_rejects_untrusted_tls_before_provider_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = serve_proxy(listener, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let result = execute(
        transport(Arc::default(), port, false, CancelToken::new(), false),
        Instant::now() + Duration::from_secs(5),
    )
    .await;
    let server_result = server.join().unwrap();
    let error = result.expect_err("untrusted provider certificate must be rejected");
    assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    assert!(
        matches!(error.failure(), TransportFailure::Connect(_)),
        "{error}"
    );
    assert!(
        server_result.is_err(),
        "the TLS server must not receive provider HTTP headers"
    );
}

// Catches: a proxied connect error that names no proxy, so a failure is blamed on the provider address.
#[tokio::test]
async fn explicit_proxy_connect_failure_names_the_proxy_address_and_route() {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let error = execute(
        transport(Arc::default(), port, false, CancelToken::new(), false),
        Instant::now() + Duration::from_secs(5),
    )
    .await
    .err()
    .unwrap();
    let TransportFailure::Connect(detail) = error.failure() else {
        panic!("expected a connection failure, got {error:?}");
    };
    assert!(detail.contains(&format!("127.0.0.1:{port}")), "{detail}");
    assert!(detail.contains("IPv4"), "{detail}");
    assert!(detail.contains("proxy: yes"), "{detail}");
}
