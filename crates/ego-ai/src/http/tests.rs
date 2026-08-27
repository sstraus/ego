use super::*;
use ego_core::permit::{
    HttpHeader, HttpRequest, HttpRequestDigest, HttpRoute, HttpScheme, NetworkTarget,
};
use reqwest::dns::{Name, Resolve};
use std::net::IpAddr;
use std::str::FromStr;
use url::Url;

async fn resolve(host: &str) -> Result<Vec<SocketAddr>, String> {
    LoopbackResolver
        .resolve(Name::from_str(host).expect("a valid DNS name"))
        .await
        .map(|addrs| addrs.collect())
        .map_err(|e| e.to_string())
}

#[tokio::test]
async fn the_offline_resolver_answers_for_localhost_from_a_constant() {
    for spelling in ["localhost", "localhost."] {
        let addrs = resolve(spelling)
            .await
            .unwrap_or_else(|e| panic!("{spelling} should resolve: {e}"));
        assert_eq!(
            addrs.iter().map(|a| a.ip()).collect::<Vec<_>>(),
            vec![IpAddr::from([127, 0, 0, 1])],
            "{spelling} must map to the loopback address and nothing else"
        );
    }
}

/// The name the admission table already refuses. It is refused here too,
/// because a single check is a single place to get it wrong — and because
/// this is the layer that makes "no DNS query leaves" a fact about the
/// program rather than a claim about its callers.
#[tokio::test]
async fn the_offline_resolver_refuses_every_other_name_rather_than_asking() {
    for host in ["api.openai.com", "evil.localhost", "localhost.evil.com"] {
        let err = resolve(host)
            .await
            .expect_err("{host} must not be resolved offline");
        assert!(
            err.contains(host),
            "the refusal must name {host}, got: {err}"
        );
    }
}

fn route(host: &str, port: u16) -> HttpRoute {
    HttpRoute::new(
        HttpScheme::Https,
        NetworkTarget::new(host, port).expect("valid target"),
    )
    .expect("valid route")
}

fn request(path: &str, body: &[u8]) -> HttpRequest {
    request_with(
        "POST",
        path,
        vec![HttpHeader::new("x-test", b"raw\xff".to_vec()).expect("header")],
        body,
    )
}

fn request_with(method: &str, path: &str, headers: Vec<HttpHeader>, body: &[u8]) -> HttpRequest {
    HttpRequest::new(method, path, headers, body.to_vec()).expect("valid request")
}

#[test]
fn admitted_profile_canonicalizes_routes_and_fingerprints_the_whole_route_set() {
    let first = route("api.example.test", 443);
    let second = route("login.example.test", 443);
    let a = AdmittedHttpProfile::try_new("provider", vec![first.clone(), second.clone()])
        .expect("profile");
    let reordered =
        AdmittedHttpProfile::try_new("provider", vec![second, first.clone()]).expect("profile");
    assert_eq!(a.fingerprint, reordered.fingerprint);

    let changed = AdmittedHttpProfile::try_new("provider", vec![first, route("other.test", 443)])
        .expect("profile");
    assert_ne!(a.fingerprint, changed.fingerprint);
    assert_eq!(a.fingerprint.id(), "provider");
}

#[test]
fn admitted_profile_rejects_duplicate_routes_and_empty_ids() {
    let endpoint = route("api.example.test", 443);
    assert!(AdmittedHttpProfile::try_new("provider", vec![]).is_err());
    assert!(AdmittedHttpProfile::try_new("", vec![endpoint.clone()]).is_err());
    assert!(AdmittedHttpProfile::try_new("provider", vec![endpoint.clone(), endpoint]).is_err());
}

#[test]
fn profile_contains_exact_scheme_host_and_port() {
    let profile = AdmittedHttpProfile::try_new("provider", vec![route("api.example.test", 443)])
        .expect("profile");
    let accepted = profile.prepare(
        Url::parse("https://api.example.test/v1/chat?stream=true").expect("url"),
        request("/v1/chat?stream=true", b"prompt"),
    );
    assert!(accepted.is_ok());

    for url in [
        "http://api.example.test/v1/chat",
        "https://api.example.test:8443/v1/chat",
        "https://other.example.test/v1/chat",
    ] {
        assert!(
            profile
                .prepare(
                    Url::parse(url).expect("url"),
                    request("/v1/chat", b"prompt")
                )
                .is_err(),
            "{url} must not be contained by the admitted route"
        );
    }
}

#[test]
fn profile_refuses_ambiguous_urls_without_echoing_sensitive_input() {
    let profile = AdmittedHttpProfile::try_new("provider", vec![route("api.example.test", 443)])
        .expect("profile");
    let cases = [
        (
            "https://user:sk-live-secret@api.example.test/v1/chat",
            "/v1/chat",
            HttpProfileError::UserInfo,
        ),
        (
            "https://api.example.test/v1/chat#fragment",
            "/v1/chat",
            HttpProfileError::Fragment,
        ),
        (
            "ftp://api.example.test/v1/chat",
            "/v1/chat",
            HttpProfileError::UnsupportedScheme,
        ),
        (
            "https://api.example.test/v1/chat",
            "/v1/other",
            HttpProfileError::RequestTargetMismatch,
        ),
    ];
    for (url, path, expected) in cases {
        let err =
            match profile.prepare(Url::parse(url).expect("test URL"), request(path, b"prompt")) {
                Err(err) => err,
                Ok(_) => panic!("{url} must be refused"),
            };
        assert_eq!(err, expected);
        let diagnostic = format!("{err:?}");
        assert!(!diagnostic.contains("sk-live-secret"));
        assert!(!diagnostic.contains("/v1/other"));
    }
}

#[test]
fn request_digest_changes_for_every_wire_material_and_never_prints_secret_bytes() {
    let header = |name: &str, value: &[u8]| HttpHeader::new(name, value.to_vec()).expect("header");
    let baseline = request_with(
        "POST",
        "/v1/chat?stream=true",
        vec![header("x-repeated", b"one"), header("x-repeated", b"two")],
        b"prompt",
    );
    let digest = HttpRequestDigest::of(&baseline);
    let variants = [
        request_with(
            "PUT",
            "/v1/chat?stream=true",
            vec![header("x-repeated", b"one"), header("x-repeated", b"two")],
            b"prompt",
        ),
        request_with(
            "POST",
            "/v1/other?stream=true",
            vec![header("x-repeated", b"one"), header("x-repeated", b"two")],
            b"prompt",
        ),
        request_with(
            "POST",
            "/v1/chat?stream=false",
            vec![header("x-repeated", b"one"), header("x-repeated", b"two")],
            b"prompt",
        ),
        request_with(
            "POST",
            "/v1/chat?stream=true",
            vec![header("x-repeated", b"two"), header("x-repeated", b"one")],
            b"prompt",
        ),
        request_with(
            "POST",
            "/v1/chat?stream=true",
            vec![header("x-repeated", b"one"), header("x-repeated", b"three")],
            b"prompt",
        ),
        request_with(
            "POST",
            "/v1/chat?stream=true",
            vec![header("x-repeated", b"one"), header("x-repeated", b"two")],
            b"changed",
        ),
    ];
    for variant in variants {
        assert_ne!(digest, HttpRequestDigest::of(&variant));
    }
    let raw_changed = request_with(
        "POST",
        "/v1/chat?stream=true",
        vec![header("x-test", b"raw\xfe")],
        b"prompt",
    );
    assert_ne!(
        HttpRequestDigest::of(&request("/v1/chat?stream=true", b"prompt")),
        HttpRequestDigest::of(&raw_changed)
    );

    let with_secret = request_with(
        "POST",
        "/v1/chat?stream=true",
        vec![header("authorization", b"Bearer sk-live-secret")],
        b"prompt",
    );
    let diagnostic = format!("{:?}", HttpRequestDigest::of(&with_secret));
    assert!(!diagnostic.contains("prompt"));
    assert!(!diagnostic.contains("Bearer"));
    assert!(!diagnostic.contains("sk-live-secret"));
}

#[test]
fn prepared_capability_binds_target_digest_and_is_consumed_into_parts() {
    let profile = AdmittedHttpProfile::try_new("provider", vec![route("api.example.test", 443)])
        .expect("profile");
    let prepared = profile
        .prepare(
            Url::parse("https://api.example.test/v1/chat").expect("url"),
            request("/v1/chat", b"prompt"),
        )
        .expect("prepared call");
    assert_eq!(prepared.profile().id(), "provider");
    assert_eq!(prepared.route().scheme(), HttpScheme::Https);
    assert_eq!(prepared.route().target().host(), "api.example.test");
    assert_eq!(prepared.route().target().port(), 443);
    assert_eq!(
        prepared.request_digest(),
        &HttpRequestDigest::of(&request("/v1/chat", b"prompt"))
    );

    let (fingerprint, route, connect, actual) = prepared.into_parts();
    assert_eq!(fingerprint.id(), "provider");
    assert_eq!(route.target().host(), "api.example.test");
    assert_eq!(connect.target().host(), "api.example.test");
    assert_eq!(connect.request_digest(), &HttpRequestDigest::of(&actual));
    assert_eq!(actual.body(), b"prompt");
}

#[test]
fn prepared_capability_refuses_target_and_digest_drift_before_io() {
    let profile = AdmittedHttpProfile::try_new("provider", vec![route("api.example.test", 443)])
        .expect("profile");
    assert!(
        profile
            .prepare(
                Url::parse("https://other.example.test/v1/chat").expect("url"),
                request("/v1/chat", b"prompt"),
            )
            .is_err()
    );
    let first = profile
        .prepare(
            Url::parse("https://api.example.test/v1/chat").expect("url"),
            request("/v1/chat", b"prompt"),
        )
        .expect("prepared call");
    let second = profile
        .prepare(
            Url::parse("https://api.example.test/v1/chat").expect("url"),
            request("/v1/chat", b"changed"),
        )
        .expect("fresh capability");
    assert_ne!(first.request_digest(), second.request_digest());
}
