use super::*;
use ego_core::permit::{
    HttpHeader, HttpRequest as CoreHttpRequest, HttpRequestDigest, HttpRoute, HttpScheme,
    NetworkTarget, OneHopResponse, TransportError, TransportFailure,
};
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use url::Url;

fn route(host: &str, port: u16) -> HttpRoute {
    HttpRoute::new(
        HttpScheme::Https,
        NetworkTarget::new(host, port).expect("valid target"),
    )
    .expect("valid route")
}

fn request(path: &str, body: &[u8]) -> CoreHttpRequest {
    request_with(
        "POST",
        path,
        vec![HttpHeader::new("x-test", b"raw\xff".to_vec()).expect("header")],
        body,
    )
}

fn request_with(
    method: &str,
    path: &str,
    headers: Vec<HttpHeader>,
    body: &[u8],
) -> CoreHttpRequest {
    CoreHttpRequest::new(method, path, headers, body.to_vec()).expect("valid request")
}

#[derive(Clone)]
struct RecordedHop {
    profile: HttpProfileFingerprint,
    route: HttpRoute,
    digest: HttpRequestDigest,
    path: String,
}

struct RecordingHttp {
    begins: Arc<Mutex<usize>>,
    calls: Arc<Mutex<Vec<RecordedHop>>>,
    responses: Arc<Mutex<VecDeque<Result<OneHopResponse, TransportError>>>>,
}

impl RecordingHttp {
    fn new(responses: Vec<Result<OneHopResponse, TransportError>>) -> Self {
        Self {
            begins: Arc::new(Mutex::new(0)),
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(responses.into_iter().collect())),
        }
    }

    fn begin_count(&self) -> usize {
        *self.begins.lock().expect("begin count")
    }

    fn calls(&self) -> Vec<RecordedHop> {
        self.calls.lock().expect("calls").clone()
    }
}

struct RecordingHttpCall {
    calls: Arc<Mutex<Vec<RecordedHop>>>,
    responses: Arc<Mutex<VecDeque<Result<OneHopResponse, TransportError>>>>,
}

impl Http for RecordingHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        *self.begins.lock().expect("begin count") += 1;
        Box::new(RecordingHttpCall {
            calls: Arc::clone(&self.calls),
            responses: Arc::clone(&self.responses),
        })
    }
}

impl HttpCall for RecordingHttpCall {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (profile, route, connect, request) = call.into_parts();
        self.calls.lock().expect("calls").push(RecordedHop {
            profile,
            route,
            digest: connect.request_digest().clone(),
            path: request.path().to_owned(),
        });
        let response = self
            .responses
            .lock()
            .expect("responses")
            .pop_front()
            .expect("logical call response");
        Box::pin(async move { response })
    }
}

fn profile_for(host: &str) -> AdmittedHttpProfile {
    AdmittedHttpProfile::try_new("provider", vec![route(host, 443)]).expect("profile")
}

fn response(status: u16, headers: Vec<HttpHeader>, body: Vec<Vec<u8>>) -> OneHopResponse {
    OneHopResponse::streaming_response(
        status,
        headers,
        Box::pin(stream::iter(body.into_iter().map(Ok))),
    )
}

fn redirect(location: impl Into<Vec<u8>>) -> OneHopResponse {
    OneHopResponse::redirect(307, Vec::new(), location.into())
}

async fn execute_profile(
    profile: &AdmittedHttpProfile,
    http: &RecordingHttp,
    request: crate::http::HttpRequest,
) -> Result<HttpResponse, HttpError> {
    profile.execute(http, request).await
}

fn provider_request(url: &str) -> crate::http::HttpRequest {
    crate::http::HttpRequest {
        url: Url::parse(url).expect("URL"),
        method: Method::Post,
        headers: vec![HttpHeader::new("x-test", b"raw".to_vec()).expect("header")],
        body: Some(b"prompt".to_vec()),
    }
}

fn logical_request(path: &str, body: &[u8]) -> CoreHttpRequest {
    request_with(
        "POST",
        path,
        vec![HttpHeader::new("x-test", b"raw".to_vec()).expect("header")],
        body,
    )
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

#[tokio::test]
async fn logical_call_begins_once_consumes_one_capability_and_streams_response() {
    let profile = profile_for("api.example.test");
    let headers = vec![
        HttpHeader::new("x-duplicate", b"first".to_vec()).expect("header"),
        HttpHeader::new("x-duplicate", b"second\xff".to_vec()).expect("header"),
    ];
    let expected_headers = headers.clone();
    let http = RecordingHttp::new(vec![Ok(response(
        200,
        headers,
        vec![b"first body".to_vec(), b"second body".to_vec()],
    ))]);
    let mut result = execute_profile(
        &profile,
        &http,
        provider_request("https://api.example.test/v1/chat"),
    )
    .await
    .expect("logical HTTP call");

    assert_eq!(http.begin_count(), 1);
    let calls = http.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].profile, profile.fingerprint);
    assert_eq!(calls[0].route, route("api.example.test", 443));
    assert_eq!(calls[0].path, "/v1/chat");
    assert_eq!(
        calls[0].digest,
        HttpRequestDigest::of(&logical_request("/v1/chat", b"prompt"))
    );
    assert_eq!(result.status, 200);
    assert_eq!(result.headers, expected_headers);
    let first = result
        .body
        .next()
        .await
        .expect("first body")
        .expect("chunk");
    let second = result
        .body
        .next()
        .await
        .expect("second body")
        .expect("chunk");
    assert_eq!(first, bytes::Bytes::from_static(b"first body"));
    assert_eq!(second, bytes::Bytes::from_static(b"second body"));
    assert!(result.body.next().await.is_none());
}

#[tokio::test]
async fn admitted_redirect_prepares_fresh_capability_on_same_call_handle() {
    let profile = profile_for("api.example.test");
    let http = RecordingHttp::new(vec![
        Ok(redirect(b"/next")),
        Ok(response(200, Vec::new(), vec![b"done".to_vec()])),
    ]);
    let result = execute_profile(
        &profile,
        &http,
        provider_request("https://api.example.test/start"),
    )
    .await
    .expect("redirected logical call");

    assert_eq!(result.status, 200);
    assert_eq!(http.begin_count(), 1);
    let calls = http.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].profile, profile.fingerprint);
    assert_eq!(calls[1].profile, profile.fingerprint);
    assert_eq!(calls[0].route, route("api.example.test", 443));
    assert_eq!(calls[1].route, route("api.example.test", 443));
    assert_eq!(calls[0].path, "/start");
    assert_eq!(calls[1].path, "/next");
    assert_ne!(calls[0].digest, calls[1].digest);
}

#[tokio::test]
async fn admitted_redirect_chain_has_no_arbitrary_hop_cap() {
    let profile = profile_for("api.example.test");
    let mut responses = (1..=11)
        .map(|step| Ok(redirect(format!("/step{step}"))))
        .collect::<Vec<_>>();
    responses.push(Ok(response(200, Vec::new(), vec![b"done".to_vec()])));
    let http = RecordingHttp::new(responses);
    let result = execute_profile(
        &profile,
        &http,
        provider_request("https://api.example.test/step0"),
    )
    .await
    .expect("long redirect chain");

    assert_eq!(result.status, 200);
    assert_eq!(http.begin_count(), 1);
    let calls = http.calls();
    assert_eq!(calls.len(), 12);
    for (step, call) in calls.iter().enumerate() {
        assert_eq!(call.path, format!("/step{step}"));
    }
}

#[tokio::test]
async fn canonical_request_identity_cycle_refuses_before_repeating_effect() {
    let profile = profile_for("api.example.test");
    let http = RecordingHttp::new(vec![Ok(redirect(b"/b")), Ok(redirect(b"/a"))]);
    let error = match execute_profile(
        &profile,
        &http,
        provider_request("https://api.example.test/a"),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("request cycle must refuse"),
    };

    assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
    assert_eq!(http.begin_count(), 1);
    assert_eq!(http.calls().len(), 2);
}

#[tokio::test]
async fn unadmitted_redirect_refuses_before_second_hop_and_keeps_delivery() {
    let profile = profile_for("api.example.test");
    let http = RecordingHttp::new(vec![Ok(redirect(b"https://evil.example.test/next"))]);
    let error = match execute_profile(
        &profile,
        &http,
        provider_request("https://api.example.test/start"),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("unadmitted redirect must refuse"),
    };

    assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
    assert_eq!(http.begin_count(), 1);
    assert_eq!(http.calls().len(), 1);
    assert_eq!(http.calls()[0].path, "/start");
}

#[tokio::test]
async fn later_definitely_unsent_hop_error_is_promoted_after_response() {
    let profile = profile_for("api.example.test");
    let later_error = TransportError::definitely_unsent(TransportFailure::Connect(
        "socket was not opened".to_owned(),
    ));
    let http = RecordingHttp::new(vec![Ok(redirect(b"/next")), Err(later_error)]);
    let error = match execute_profile(
        &profile,
        &http,
        provider_request("https://api.example.test/start"),
    )
    .await
    {
        Err(error) => error,
        Ok(_) => panic!("later hop must fail"),
    };

    assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
    assert_eq!(http.begin_count(), 1);
    assert_eq!(http.calls().len(), 2);
}

#[tokio::test]
async fn response_body_error_preserves_typed_delivery() {
    let profile = profile_for("api.example.test");
    let body_error = TransportError::possibly_sent(TransportFailure::ResponseBody(
        "body stream stopped".to_owned(),
    ));
    let streamed = OneHopResponse::streaming_response(
        200,
        Vec::new(),
        Box::pin(stream::iter(vec![Ok(b"part".to_vec()), Err(body_error)])),
    );
    let http = RecordingHttp::new(vec![Ok(streamed)]);
    let mut result = execute_profile(
        &profile,
        &http,
        provider_request("https://api.example.test/start"),
    )
    .await
    .expect("body-stream response");

    assert_eq!(result.status, 200);
    assert_eq!(
        result
            .body
            .next()
            .await
            .expect("body chunk")
            .expect("chunk"),
        bytes::Bytes::from_static(b"part")
    );
    let error = result
        .body
        .next()
        .await
        .expect("body error")
        .expect_err("typed body error");
    assert_eq!(error.delivery(), DeliveryCertainty::PossiblySent);
}

/// Catches the protocol adapter erasing transport causes or the admitted host.
#[tokio::test]
async fn connect_and_timeout_diagnostics_retain_host_without_request_secrets() {
    for failure in [
        TransportFailure::Connect("client error: TLS handshake: certificate verify failed".into()),
        TransportFailure::TimedOut,
    ] {
        let http = RecordingHttp::new(vec![Err(TransportError::definitely_unsent(failure))]);
        let error = match execute_profile(
            &profile_for("api.example.test"),
            &http,
            provider_request("https://api.example.test/private?key=sk-live-secret"),
        )
        .await
        {
            Err(error) => error,
            Ok(_) => panic!("transport failure must remain a failure"),
        };
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("api.example.test"));
        assert!(!diagnostic.contains("private"));
        assert!(!diagnostic.contains("sk-live-secret"));
        if diagnostic.contains("connection failed") {
            assert!(diagnostic.contains("TLS handshake: certificate verify failed"));
        } else {
            assert!(diagnostic.contains("timed out"));
        }
        assert_eq!(error.kind(), ErrorKind::Transient);
        assert_eq!(error.delivery(), DeliveryCertainty::DefinitelyUnsent);
    }
}

/// Catches an authenticated cache key silently using the secret-blind public digest.
#[test]
fn authenticated_reuse_digest_distinguishes_secrets_without_changing_public_commitments() {
    let request = |secret: &[u8]| request_with("GET", "/models", vec![bearer_header(secret)], b"");
    let first = request(b"first-account-secret");
    let second = request(b"second-account-secret");
    assert_eq!(
        HttpRequestDigest::of(&first),
        HttpRequestDigest::of(&second)
    );
    let identity = HttpRequestDigest::for_authenticated_reuse(&first);
    assert_eq!(
        identity,
        HttpRequestDigest::for_authenticated_reuse(&request(b"first-account-secret"))
    );
    assert_ne!(
        identity,
        HttpRequestDigest::for_authenticated_reuse(&second)
    );
    let diagnostic = format!("{identity:?}");
    assert!(!diagnostic.contains("first-account-secret"));
    assert!(!diagnostic.contains("Bearer"));
}
