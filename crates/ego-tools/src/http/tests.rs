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
    DnsResolution, GuardedHttp, HttpRequest, HttpRoute, HttpScheme, NetworkTarget, OneHop,
    OneHopResponse, PreparedConnect, Resolver, TransportError, TransportFailure,
};
use ego_core::turn::CancelToken;
use futures::future::BoxFuture;

use super::{OneShotHttpTransport, canonical_is_loopback};
use crate::exec::HttpCallTimeout;

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
        _deadline: Option<Instant>,
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
    fn execute(
        &self,
        connect: ego_core::permit::ResolvedConnect,
        request: HttpRequest,
        deadline: Option<Instant>,
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
        true,
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
            Some(Instant::now() + Duration::from_secs(5)),
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
            Some(Instant::now() + Duration::from_secs(5)),
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
        true,
    );
    let target = NetworkTarget::new("localhost", 80).expect("localhost target");
    let resolution = transport
        .resolve(target, None)
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
        true,
    );
    let error = transport
        .resolve(
            NetworkTarget::new("api.example.test", 443).expect("named target"),
            None,
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
