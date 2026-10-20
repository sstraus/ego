use std::future;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use ego_core::event::domain::CancelCause;
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::{
    CanonPath, Confinement, DnsResolution, GuardedHttp, HttpRequest, HttpRoute, HttpScheme,
    NetworkTarget, OneHop, OneHopResponse, PreparedConnect, Resolver, RunConfinement,
    TransportError, TransportFailure,
};
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;

use super::{OneShotHttpTransport, canonical_is_loopback};
use crate::exec::HttpCallTimeout;

const TEST_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/http-tests");

fn offline_confinement() -> RunConfinement {
    RunConfinement::new(Confinement::Off, true, CanonPath::new(TEST_ROOT))
        .expect("offline test confinement")
}

struct FixedResolver {
    calls: Arc<AtomicUsize>,
    address: IpAddr,
}

impl FixedResolver {
    fn new(address: IpAddr) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            address,
        }
    }
}

impl Resolver for FixedResolver {
    fn resolve(
        &self,
        target: NetworkTarget,
        _deadline: Instant,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Box::pin(future::ready(
            DnsResolution::new(target, vec![self.address]).map_err(|_| {
                TransportError::definitely_unsent(TransportFailure::Resolution(
                    "invalid fixed resolver fixture".to_owned(),
                ))
            }),
        ))
    }
}

struct DelegatingHop {
    transport: Arc<OneShotHttpTransport>,
}

impl OneHop for DelegatingHop {
    fn begin_call(&self) -> Instant {
        Instant::now() + Duration::from_secs(60)
    }
    fn execute(
        &self,
        connect: ego_core::permit::ResolvedConnect,
        request: HttpRequest,
        deadline: Instant,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        self.transport.execute(connect, request, deadline)
    }
}

fn offline_transport(
    resolver: Arc<dyn Resolver>,
    cancel: CancelToken,
) -> Arc<OneShotHttpTransport> {
    Arc::new(OneShotHttpTransport::new(
        resolver,
        cancel,
        HttpCallTimeout::default(),
        None,
        offline_confinement(),
    ))
}

async fn one_hop_with_selected(
    host: &str,
    selected: IpAddr,
) -> Result<OneHopResponse, TransportError> {
    let target = NetworkTarget::new(host, 80).expect("network target");
    let route = HttpRoute::new(HttpScheme::Http, target.clone()).expect("HTTP route");
    let request = HttpRequest::get("/").expect("HTTP request");
    let connect = PreparedConnect::new(target, &request);
    let resolver = Arc::new(FixedResolver::new(selected));
    let transport = offline_transport(Arc::new(FixedResolver::new(selected)), CancelToken::new());
    let hop = Arc::new(DelegatingHop {
        transport: Arc::clone(&transport),
    });
    GuardedHttp::new(resolver.clone(), hop.clone())
        .execute_admitted(
            route,
            connect,
            request,
            Instant::now() + Duration::from_secs(5),
        )
        .await
}

async fn cancelled_literal(host: &str) -> Result<OneHopResponse, TransportError> {
    let target = NetworkTarget::new(host, 80).expect("literal network target");
    let route = HttpRoute::new(HttpScheme::Http, target.clone()).expect("HTTP route");
    let request = HttpRequest::get("/").expect("HTTP request");
    let connect = PreparedConnect::new(target, &request);
    let cancel = CancelToken::new();
    cancel.cancel(CancelCause::User);
    let transport = offline_transport(
        Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        cancel,
    );

    GuardedHttp::new(transport.clone(), transport.clone())
        .execute_admitted(
            route,
            connect,
            request,
            Instant::now() + Duration::from_secs(5),
        )
        .await
}

fn assert_offline_refusal(result: Result<OneHopResponse, TransportError>) {
    let error = result.expect_err("offline non-loopback hop must be refused");
    assert!(matches!(error.failure(), TransportFailure::Offline));
    assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
}

#[tokio::test]
async fn offline_localhost_uses_deterministic_loopback_without_injected_dns() {
    let resolver = Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))));
    let calls = Arc::clone(&resolver.calls);
    let transport = OneShotHttpTransport::new(
        resolver,
        CancelToken::new(),
        HttpCallTimeout::default(),
        None,
        offline_confinement(),
    );
    let target = NetworkTarget::new("localhost", 80).expect("localhost target");
    let resolution = transport
        .resolve(target, Instant::now() + Duration::from_secs(5))
        .await
        .expect("localhost is the offline DNS exception");

    assert_eq!(resolution.target().host(), "localhost");
    assert_eq!(resolution.addresses(), &[IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "offline localhost bypasses DNS"
    );
}

#[tokio::test]
async fn offline_non_local_name_is_definitely_unsent_before_dns() {
    let resolver = Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    let calls = Arc::clone(&resolver.calls);
    let transport = OneShotHttpTransport::new(
        resolver,
        CancelToken::new(),
        HttpCallTimeout::default(),
        None,
        offline_confinement(),
    );
    let error = transport
        .resolve(
            NetworkTarget::new("api.example.test", 443).expect("named target"),
            Instant::now() + Duration::from_secs(5),
        )
        .await
        .expect_err("offline named target must be refused");

    assert!(matches!(error.failure(), TransportFailure::Offline));
    assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    assert_eq!(
        calls.load(Ordering::Relaxed),
        0,
        "offline refusal bypasses DNS"
    );
}

#[test]
fn offline_loopback_classification_preserves_ipv4_ipv6_and_mapped_ipv4() {
    assert!(canonical_is_loopback(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert!(canonical_is_loopback(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    assert!(canonical_is_loopback(IpAddr::V6(Ipv6Addr::new(
        0, 0, 0, 0, 0, 0xffff, 0x7f00, 1,
    ))));
    assert!(!canonical_is_loopback(IpAddr::V4(Ipv4Addr::new(
        192, 0, 2, 10
    ))));
}

#[tokio::test]
async fn offline_admits_loopback_literal_forms_before_cancellation() {
    for host in ["127.0.0.1", "[::1]", "[::ffff:127.0.0.1]"] {
        let error = cancelled_literal(host)
            .await
            .expect_err("cancelled fixture must not connect");
        assert!(
            matches!(error.failure(), TransportFailure::Cancelled),
            "offline literal {host:?} returned {:?}",
            error.failure()
        );
        assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    }
}

#[tokio::test]
async fn offline_rejects_non_loopback_logical_and_selected_targets() {
    assert_offline_refusal(
        one_hop_with_selected("api.example.test", IpAddr::V4(Ipv4Addr::LOCALHOST)).await,
    );
    assert_offline_refusal(
        one_hop_with_selected("localhost", IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))).await,
    );
}

/// regression case — catches: a silent response bypasses the logical call
/// deadline. Both absent headers and a stalled SSE body must be bounded.
#[tokio::test]
async fn regression_case_silent_provider_is_bounded_by_the_logical_call_deadline() {
    use futures::StreamExt as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    assert_eq!(
        HttpCallTimeout::default().duration(),
        Duration::from_secs(1800)
    );
    // Prepare lazy runtime and TLS setup before the silent-response scenarios.
    // A 150 ms budget could expire during setup: TCP connected, but the
    // deadline cancelled the send before the server received any request.
    crate::exec::runtime_handle();
    let _prepared_client = reqwest::Client::builder().no_proxy().build().unwrap();
    for send_headers in [false, true] {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (release, hold) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            if send_headers {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                stream.flush().await.unwrap();
            }
            let _ = hold.await;
        });
        let target = NetworkTarget::new("localhost", port).unwrap();
        let route = HttpRoute::new(HttpScheme::Http, target.clone()).unwrap();
        let request = HttpRequest::get("/").unwrap();
        let connect = PreparedConnect::new(target, &request);
        let transport = offline_transport(
            Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::LOCALHOST))),
            CancelToken::new(),
        );
        // Client construction is per hop; leave room for it and scheduling,
        // while the outer watchdog still catches a missing call deadline.
        let deadline = Instant::now() + Duration::from_secs(1);
        let observed = tokio::time::timeout(Duration::from_secs(3), async {
            match GuardedHttp::new(transport.clone(), transport)
                .execute_admitted(route, connect, request, deadline)
                .await
            {
                Err(error) => error,
                Ok(response) => {
                    let OneHopResponse::Response { mut body, .. } = response else {
                        panic!("fixture must not redirect");
                    };
                    body.next()
                        .await
                        .expect("timeout body event")
                        .expect_err("silent body must time out")
                }
            }
        })
        .await;
        let _ = release.send(());
        server.await.unwrap();
        let error = observed.expect("silent provider must not wait forever");
        assert_eq!(error.failure(), &TransportFailure::TimedOut);
        assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
    }
}

#[tokio::test]
async fn websocket_rejects_offline_remote_before_opening_a_socket() {
    let target = NetworkTarget::new("203.0.113.1", 443).unwrap();
    let route = HttpRoute::new(HttpScheme::Https, target.clone()).unwrap();
    let request = HttpRequest::new("POST", "/codex/responses", vec![], b"{}".to_vec()).unwrap();
    let connect = PreparedConnect::new(target, &request);
    let transport = offline_transport(
        Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        CancelToken::new(),
    );
    let result = GuardedHttp::new(transport.clone(), transport)
        .open_websocket_admitted(
            route,
            connect,
            request,
            Instant::now() + Duration::from_secs(2),
        )
        .await;
    let error = result.err().expect("offline remote socket refused");
    assert_eq!(error.failure(), &TransportFailure::Offline);
    assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
}

#[tokio::test]
async fn websocket_cancelled_or_expired_run_never_connects() {
    for expired in [false, true] {
        let target = NetworkTarget::new("127.0.0.1", 9).unwrap();
        let route = HttpRoute::new(HttpScheme::Http, target.clone()).unwrap();
        let request = HttpRequest::new("POST", "/", vec![], b"{}".to_vec()).unwrap();
        let connect = PreparedConnect::new(target, &request);
        let cancel = CancelToken::new();
        if !expired {
            cancel.cancel(CancelCause::User);
        }
        let transport = offline_transport(
            Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::LOCALHOST))),
            cancel,
        );
        let deadline = if expired {
            Instant::now()
        } else {
            Instant::now() + Duration::from_secs(2)
        };
        let result = GuardedHttp::new(transport.clone(), transport)
            .open_websocket_admitted(route, connect, request, deadline)
            .await;
        let error = result.err().expect("ended authority refused");
        assert_eq!(
            error.failure(),
            if expired {
                &TransportFailure::TimedOut
            } else {
                &TransportFailure::Cancelled
            }
        );
        assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    }
}

#[tokio::test]
async fn websocket_request_substitution_is_refused_before_dns() {
    let target = NetworkTarget::new("provider.test", 443).unwrap();
    let route = HttpRoute::new(HttpScheme::Https, target.clone()).unwrap();
    let request = HttpRequest::new("POST", "/", vec![], b"{}".to_vec()).unwrap();
    let connect = PreparedConnect::new(target, &request);
    let changed = HttpRequest::new("POST", "/other", vec![], b"{}".to_vec()).unwrap();
    let resolver = Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    let transport = offline_transport(resolver.clone(), CancelToken::new());
    let result = GuardedHttp::new(resolver.clone(), transport)
        .open_websocket_admitted(
            route,
            connect,
            changed,
            Instant::now() + Duration::from_secs(2),
        )
        .await;
    assert_eq!(
        result.err().unwrap().failure(),
        &TransportFailure::AuthorityMismatch
    );
    assert_eq!(resolver.calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn websocket_checks_each_request_and_keeps_destination_pinned() {
    use futures::StreamExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = async_tungstenite::tokio::accept_async(tcp).await.unwrap();
        for expected in ["first", "second"] {
            let message = socket.next().await.unwrap().unwrap();
            assert_eq!(message.to_text().unwrap(), expected);
            socket
                .send(async_tungstenite::tungstenite::Message::Text(
                    "reply".into(),
                ))
                .await
                .unwrap();
        }
        // Cancellation must close an idle socket without another provider request.
        let _ = socket.next().await;
    });
    let target = NetworkTarget::new("provider.test", port).unwrap();
    let route = HttpRoute::new(HttpScheme::Http, target.clone()).unwrap();
    let request = HttpRequest::new("POST", "/codex/responses", vec![], b"first".to_vec()).unwrap();
    let resolver = Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    let cancel = CancelToken::new();
    let confinement =
        RunConfinement::new(Confinement::Off, false, CanonPath::new(TEST_ROOT)).unwrap();
    let transport = Arc::new(OneShotHttpTransport::new(
        resolver.clone(),
        cancel.clone(),
        HttpCallTimeout::default(),
        None,
        confinement,
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket = GuardedHttp::new(resolver.clone(), transport)
        .open_websocket_admitted(
            route,
            PreparedConnect::new(target.clone(), &request),
            request.clone(),
            deadline,
        )
        .await
        .unwrap();
    let substituted = HttpRequest::new("POST", "/elsewhere", vec![], b"stolen".to_vec()).unwrap();
    assert_eq!(
        socket
            .send(
                PreparedConnect::new(target.clone(), &substituted),
                substituted,
                deadline
            )
            .await
            .unwrap_err()
            .failure(),
        &TransportFailure::AuthorityMismatch
    );
    let changed =
        HttpRequest::new("POST", "/codex/responses", vec![], b"tampered".to_vec()).unwrap();
    assert_eq!(
        socket
            .send(
                PreparedConnect::new(target.clone(), &request),
                changed,
                deadline
            )
            .await
            .unwrap_err()
            .failure(),
        &TransportFailure::AuthorityMismatch
    );
    for text in ["first", "second"] {
        let request =
            HttpRequest::new("POST", "/codex/responses", vec![], text.as_bytes().to_vec()).unwrap();
        socket
            .send(
                PreparedConnect::new(target.clone(), &request),
                request,
                deadline,
            )
            .await
            .unwrap();
        assert_eq!(
            socket.receive(deadline).await.unwrap(),
            Some(b"reply".to_vec())
        );
    }
    assert_eq!(resolver.calls.load(Ordering::Relaxed), 1);
    cancel.cancel(CancelCause::User);
    let request = HttpRequest::new("POST", "/codex/responses", vec![], b"late".to_vec()).unwrap();
    assert_eq!(
        socket
            .send(PreparedConnect::new(target, &request), request, deadline)
            .await
            .unwrap_err()
            .failure(),
        &TransportFailure::Cancelled
    );
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn websocket_run_expiry_closes_idle_socket_and_refuses_late_request() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        use futures::StreamExt;
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = async_tungstenite::tokio::accept_async(tcp).await.unwrap();
        let _ = socket.next().await;
    });
    let target = NetworkTarget::new("127.0.0.1", port).unwrap();
    let route = HttpRoute::new(HttpScheme::Http, target.clone()).unwrap();
    let request = HttpRequest::new("POST", "/", vec![], b"late".to_vec()).unwrap();
    let resolver = Arc::new(FixedResolver::new(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    let confinement =
        RunConfinement::new(Confinement::Off, false, CanonPath::new(TEST_ROOT)).unwrap();
    let transport = Arc::new(OneShotHttpTransport::new(
        resolver.clone(),
        CancelToken::new(),
        HttpCallTimeout::default(),
        Some(Instant::now() + Duration::from_millis(100)),
        confinement,
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut socket = GuardedHttp::new(resolver, transport)
        .open_websocket_admitted(
            route,
            PreparedConnect::new(target.clone(), &request),
            request.clone(),
            deadline,
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        socket
            .send(PreparedConnect::new(target, &request), request, deadline)
            .await
            .unwrap_err()
            .failure(),
        &TransportFailure::TimedOut
    );
}
