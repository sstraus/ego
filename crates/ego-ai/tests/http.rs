//! The transport seam and the egress policy it enforces.
//!
//! Two kinds of test live here, and the split is the point.
//!
//! The classification table is **pure**: it opens no socket and sends no DNS
//! query, because `EgressPolicy::admit` cannot. That is not a convenience of
//! the test, it is the property under test — "no non-loopback egress" is only
//! true of a refusal if refusing cost no packets.
//!
//! The transport tests then prove the parts a pure predicate cannot: that an
//! admitted loopback daemon is actually reachable, that `localhost` is mapped
//! without a resolver, and that an admitted server cannot use a redirect to
//! walk the connection off the machine.
//!
//! The third enforcement point — the resolver — is tested next to itself, in
//! `src/http.rs`: from out here a `localhost` request looks the same whether
//! the transport's own constant answered or the system resolver did, so an
//! integration test could assert the outcome but never the mechanism.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ego_ai::http::{
    EgressPolicy, EgressRefused, Http, HttpError, HttpRequest, Method, ReqwestHttp,
};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use url::Url;

fn admit(policy: EgressPolicy, raw: &str) -> Result<(), EgressRefused> {
    let url = Url::parse(raw).unwrap_or_else(|e| panic!("{raw} should parse as a URL: {e}"));
    policy.admit(&url)
}

// --- what the policy admits, and what it names when it refuses ---

/// The table is written as "what a user would type" rather than "what the
/// parser produces", because the whole class of bugs here is the gap between
/// the two: `2130706433` *is* `127.0.0.1`, and `127.0.0.1@evil.com` is *not*.
#[test]
fn offline_admits_every_spelling_of_loopback_and_nothing_else() {
    let admitted = [
        // The four local daemons this flag exists for.
        ("http://127.0.0.1:11434/api/chat", "Ollama's default"),
        ("http://localhost:1234/v1/models", "LM Studio's default"),
        ("http://localhost:8000/v1/completions", "vLLM's default"),
        ("http://127.0.0.1:8080/completion", "llama.cpp-server"),
        // Every IPv4 spelling the URL parser folds together. A textual check
        // would refuse these while they address the local machine.
        ("http://2130706433/", "the integer spelling"),
        ("http://0177.0.0.1/", "the octal spelling"),
        ("http://0x7f.0.0.1/", "the hex spelling"),
        ("http://127.1/", "the short spelling"),
        ("http://127.255.255.254/", "the far end of 127/8"),
        // IPv6, including the v4-mapped form `Ipv6Addr::is_loopback` misses.
        ("http://[::1]:8080/", "the IPv6 loopback"),
        ("http://[0:0:0:0:0:0:0:1]/", "its expanded spelling"),
        ("http://[::ffff:127.0.0.1]/", "the v4-mapped loopback"),
        // The one reserved name, in the spellings the parser normalises.
        ("http://LOCALHOST/", "localhost, shouted"),
        ("http://localhost./", "localhost, fully qualified"),
        ("https://localhost:8443/v1", "localhost over TLS"),
    ];
    for (raw, why) in admitted {
        admit(EgressPolicy::LoopbackOnly, raw)
            .unwrap_or_else(|e| panic!("{raw} ({why}) should be admitted offline, got {e}"));
    }

    let refused = [
        // Not loopback, however local it feels.
        (
            "http://192.168.1.5:11434/",
            EgressRefused::NotLoopbackAddress {
                host: "192.168.1.5".to_string(),
            },
        ),
        (
            "http://10.0.0.1/",
            EgressRefused::NotLoopbackAddress {
                host: "10.0.0.1".to_string(),
            },
        ),
        // The unspecified addresses. They often reach a local listener, but
        // they do not *name* the loopback interface, so they fail closed.
        (
            "http://0.0.0.0/",
            EgressRefused::NotLoopbackAddress {
                host: "0.0.0.0".to_string(),
            },
        ),
        (
            "http://[::]/",
            EgressRefused::NotLoopbackAddress {
                host: "::".to_string(),
            },
        ),
        // A v4-mapped address that is not loopback: `to_canonical` must not
        // wave through everything it unwraps.
        (
            "http://[::ffff:8.8.8.8]/",
            EgressRefused::NotLoopbackAddress {
                // Named as written, not as canonicalised: the user typed a
                // v6 literal and a denial that answered about a different
                // spelling would be answering a question nobody asked.
                host: "::ffff:8.8.8.8".to_string(),
            },
        ),
        // Userinfo is not a host. The refusal must name `evil.com`, which is
        // where the packets would have gone.
        (
            "http://127.0.0.1@evil.com/",
            EgressRefused::NameNeedsResolution {
                host: "evil.com".to_string(),
            },
        ),
        // Names that resolve to loopback, or merely look like they do. Each
        // needs a resolver to judge, and asking is the egress being refused.
        (
            "http://127.0.0.1.nip.io/",
            EgressRefused::NameNeedsResolution {
                host: "127.0.0.1.nip.io".to_string(),
            },
        ),
        (
            "http://localhost.evil.com/",
            EgressRefused::NameNeedsResolution {
                host: "localhost.evil.com".to_string(),
            },
        ),
        (
            "http://evil.localhost/",
            EgressRefused::NameNeedsResolution {
                host: "evil.localhost".to_string(),
            },
        ),
        (
            "http://evillocalhost/",
            EgressRefused::NameNeedsResolution {
                host: "evillocalhost".to_string(),
            },
        ),
        (
            "http://host.docker.internal:11434/",
            EgressRefused::NameNeedsResolution {
                host: "host.docker.internal".to_string(),
            },
        ),
        (
            "https://api.openai.com/v1/chat/completions",
            EgressRefused::NameNeedsResolution {
                host: "api.openai.com".to_string(),
            },
        ),
    ];
    for (raw, expected) in refused {
        let err = admit(EgressPolicy::LoopbackOnly, raw)
            .err()
            .unwrap_or_else(|| panic!("{raw} should be refused offline"));
        assert_eq!(err, expected, "{raw} was refused for the wrong reason");
    }
}

/// The scheme check is not part of the offline posture: `file:///etc/passwd`
/// is not a local HTTP request, it is not an HTTP request. Refusing it only
/// when offline would make an unrestricted transport try to speak HTTP to a
/// path.
#[test]
fn a_scheme_this_transport_does_not_speak_is_refused_under_every_policy() {
    for policy in [EgressPolicy::Unrestricted, EgressPolicy::LoopbackOnly] {
        assert_eq!(
            admit(policy, "file:///etc/passwd").err(),
            Some(EgressRefused::NotHttp {
                scheme: "file".to_string()
            }),
            "{policy:?} should refuse a file URL"
        );
        assert_eq!(
            admit(policy, "ftp://example.com/x").err(),
            Some(EgressRefused::NotHttp {
                scheme: "ftp".to_string()
            }),
            "{policy:?} should refuse an ftp URL"
        );
    }
}

/// The mirror of the offline table: the *only* thing an unrestricted policy
/// refuses is a scheme it cannot speak. If this test ever starts refusing a
/// remote host, `--offline` has leaked into the default posture.
#[test]
fn unrestricted_admits_the_destinations_offline_refuses() {
    for raw in [
        "https://api.openai.com/v1/chat/completions",
        "http://192.168.1.5:11434/",
        "http://host.docker.internal:11434/",
        "http://0.0.0.0/",
        "http://127.0.0.1:11434/api/chat",
    ] {
        admit(EgressPolicy::Unrestricted, raw)
            .unwrap_or_else(|e| panic!("{raw} should be admitted unrestricted, got {e}"));
    }
}

// --- and what the transport actually does with an admitted destination ---

/// A one-shot HTTP/1.1 server on a loopback port the kernel chooses.
///
/// Port zero rather than a fixed one, and the accept loop is running before
/// the request is made, so nothing here depends on a sleep or on a port being
/// free. The accept count is what proves a *second* connection was never
/// attempted.
struct Listener {
    port: u16,
    accepted: Arc<AtomicUsize>,
}

impl Listener {
    async fn serving(response: &'static str) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("binding a loopback port should succeed");
        let port = listener
            .local_addr()
            .expect("bound socket has an address")
            .port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                // Drain the request head so the client is not reset before it
                // has read the response.
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while socket.read_exact(&mut byte).await.is_ok() {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        Self { port, accepted }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// Note what this helper does *not* do: it never calls `admit`. The request
/// carries a plain URL and the transport is what checks it — which is the
/// difference between `--offline` being enforced at the seam and being a
/// habit every call site has to remember.
async fn get(
    http: &ReqwestHttp,
    url: &str,
) -> Result<(u16, Vec<(String, String)>, String), HttpError> {
    let response = http
        .execute(HttpRequest {
            url: Url::parse(url).expect("test URL parses"),
            method: Method::Get,
            headers: Vec::new(),
            body: None,
        })
        .await?;
    let status = response.status;
    let headers = response.headers;
    let mut body = Vec::new();
    let mut stream = response.body;
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk?);
    }
    Ok((status, headers, String::from_utf8_lossy(&body).into_owned()))
}

const OK_RESPONSE: &str = "HTTP/1.1 200 OK\r\n\
     content-type: text/event-stream\r\n\
     content-length: 5\r\n\
     connection: close\r\n\
     \r\n\
     hello";

/// The claim in the plan is that enforcement lives *at the seam*, and this is
/// what that means operationally: a caller that never consulted the policy,
/// and hands the transport a plain remote URL, is refused anyway — by name,
/// and without a packet, because the check runs before the request is built.
///
/// If this ever passes, `--offline` has become a convention: a rule that
/// holds only for the call sites that remembered it.
#[tokio::test]
async fn the_transport_refuses_a_remote_url_its_caller_never_checked() {
    let http = ReqwestHttp::new(EgressPolicy::LoopbackOnly).expect("client builds");

    let err = get(&http, "https://api.openai.com/v1/chat/completions")
        .await
        .expect_err("an offline transport must refuse a remote host");

    match err {
        HttpError::Refused(EgressRefused::NameNeedsResolution { host }) => {
            assert_eq!(host, "api.openai.com");
        }
        other => panic!("expected a named egress refusal, got {other:?}"),
    }
}

/// The test above cannot tell *which* layer refused: `api.openai.com` is a
/// name, so the transport's own resolver would refuse it even if `execute`
/// had stopped checking. This one can. `0.0.0.0` is an IP literal, so no
/// resolver is consulted at all — and it is an address the OS happily routes
/// to the loopback interface, so if `execute` did not check, the request
/// would *succeed* against a real listener.
///
/// Hence the second assertion, which is the one that matters: the listener
/// was never touched. The refusal happened before a socket, not after one.
#[tokio::test]
async fn the_transport_refuses_before_it_connects_even_when_the_address_would_work() {
    let server = Listener::serving(OK_RESPONSE).await;
    let http = ReqwestHttp::new(EgressPolicy::LoopbackOnly).expect("client builds");

    let err = get(&http, &format!("http://0.0.0.0:{}/v1/models", server.port))
        .await
        .expect_err("0.0.0.0 does not name the loopback interface");

    match err {
        HttpError::Refused(EgressRefused::NotLoopbackAddress { host }) => {
            assert_eq!(host, "0.0.0.0");
        }
        other => panic!("expected a refusal decided before connecting, got {other:?}"),
    }
    assert_eq!(
        server.accepted(),
        0,
        "a refused destination must cost no connection at all"
    );
}

/// The half of `--offline` that is easy to forget: it has to leave the local
/// daemons working, or nobody air-gapped can use the tool at all.
#[tokio::test]
async fn an_offline_transport_still_reaches_a_loopback_daemon() {
    let server = Listener::serving(OK_RESPONSE).await;
    let http = ReqwestHttp::new(EgressPolicy::LoopbackOnly).expect("client builds");

    let (status, headers, body) = get(
        &http,
        &format!("http://127.0.0.1:{}/v1/models", server.port),
    )
    .await
    .expect("a loopback request should succeed offline");

    assert_eq!(status, 200);
    assert_eq!(body, "hello");
    assert!(
        headers
            .iter()
            .any(|(n, v)| n == "content-type" && v == "text/event-stream"),
        "the wire format needs the content type to tell a stream from an error page: {headers:?}"
    );
}

/// `localhost` is admitted by name, so something has to turn it into an
/// address. This proves that something is the transport's own constant and
/// not the system resolver: the request lands on a port only this test knows.
#[tokio::test]
async fn offline_maps_localhost_itself_instead_of_asking_a_resolver() {
    let server = Listener::serving(OK_RESPONSE).await;
    let http = ReqwestHttp::new(EgressPolicy::LoopbackOnly).expect("client builds");

    let (status, _, body) = get(
        &http,
        &format!("http://localhost:{}/v1/models", server.port),
    )
    .await
    .expect("localhost should resolve to loopback offline");

    assert_eq!((status, body.as_str()), (200, "hello"));
    assert_eq!(
        server.accepted(),
        1,
        "the request should have reached the loopback listener exactly once"
    );
}

/// The hole a caller-side URL check cannot see. The first hop is loopback and
/// passes any check anyone would write; the server then answers `302
/// Location: http://192.0.2.1/` and a default HTTP client follows it — up to
/// ten times. Enforcing at the seam means every hop is re-admitted.
///
/// `192.0.2.1` is RFC 5737 TEST-NET-1, which is guaranteed not to be routed,
/// so a regression here fails as a refusal-shaped assertion rather than as a
/// connection to somebody's real host.
#[tokio::test]
async fn an_admitted_server_cannot_redirect_the_connection_off_the_machine() {
    const REDIRECT: &str = "HTTP/1.1 302 Found\r\n\
         location: http://192.0.2.1/v1/models\r\n\
         content-length: 0\r\n\
         connection: close\r\n\
         \r\n";
    let server = Listener::serving(REDIRECT).await;
    let http = ReqwestHttp::new(EgressPolicy::LoopbackOnly).expect("client builds");

    let err = get(
        &http,
        &format!("http://127.0.0.1:{}/v1/models", server.port),
    )
    .await
    .expect_err("following a redirect off the loopback interface must fail");

    match err {
        HttpError::Refused(EgressRefused::NotLoopbackAddress { host }) => {
            assert_eq!(
                host, "192.0.2.1",
                "the refusal must name the hop it refused"
            );
        }
        other => panic!("expected a named egress refusal, got {other:?}"),
    }
    assert_eq!(
        server.accepted(),
        1,
        "only the admitted first hop should ever have been connected to"
    );
}

/// An unrestricted transport follows the same redirect, which is what makes
/// the test above a statement about the *policy* rather than about redirects
/// being broken.
#[tokio::test]
async fn an_unrestricted_transport_is_free_to_follow_the_same_redirect() {
    let target = Listener::serving(OK_RESPONSE).await;
    let redirect: &'static str = Box::leak(
        format!(
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:{}/v1/models\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            target.port
        )
        .into_boxed_str(),
    );
    let front = Listener::serving(redirect).await;
    let http = ReqwestHttp::new(EgressPolicy::Unrestricted).expect("client builds");

    let (status, _, body) = get(&http, &format!("http://127.0.0.1:{}/v1/models", front.port))
        .await
        .expect("an unrestricted transport follows redirects");

    assert_eq!((status, body.as_str()), (200, "hello"));
    assert_eq!(front.accepted(), 1);
    assert_eq!(
        target.accepted(),
        1,
        "the redirect should have been followed"
    );
}

/// Proxy support is refused twice: `.no_proxy()` on the builder, and the
/// `system-proxy` feature left off so the environment-reading code is not
/// compiled in at all. The second is the stronger guarantee — absent code
/// cannot be reached — but it lives in a manifest, where a future edit
/// enabling a feature for an unrelated reason would silently undo it.
///
/// This test is that edit's alarm. It cannot be written as a runtime check:
/// `std::env::set_var` is `unsafe` in edition 2024 and this workspace denies
/// `unsafe_code`, so a test that sets `HTTP_PROXY` cannot exist here.
#[test]
fn the_manifest_never_lets_the_environment_choose_a_proxy() {
    // Comments are stripped first: the manifest *explains* why the feature
    // is off, and a test that read the explanation as the setting would fail
    // on a correct manifest and pass on one that only stopped explaining.
    let manifest: String = include_str!("../Cargo.toml")
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !manifest.contains("system-proxy"),
        "enabling reqwest's `system-proxy` feature would let HTTP_PROXY \
         redirect a request the egress policy admitted as loopback"
    );
    assert!(
        manifest.contains("default-features = false"),
        "reqwest's default features include the proxy stack; turning them on \
         would reintroduce the environment as an input to where bytes go"
    );
}
