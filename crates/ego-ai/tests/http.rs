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
use ego_ai::provider::{CredentialStrategy, Credentials, MissingCredential, Secret};
use ego_core::generator::attempt::DeliveryCertainty;
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

    assert_eq!(
        err.refusal(),
        Some(&EgressRefused::NameNeedsResolution {
            host: "api.openai.com".to_string()
        }),
        "expected a named egress refusal, got {err:?}"
    );
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

    assert_eq!(
        err.refusal(),
        Some(&EgressRefused::NotLoopbackAddress {
            host: "0.0.0.0".to_string()
        }),
        "expected a refusal decided before connecting, got {err:?}"
    );
    assert_eq!(
        server.accepted(),
        0,
        "a refused destination must cost no connection at all"
    );
    assert_eq!(
        err.delivery(),
        DeliveryCertainty::DefinitelyUnsent,
        "nothing was connected to, so nothing could have been billed"
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

    assert_eq!(
        err.refusal(),
        Some(&EgressRefused::NotLoopbackAddress {
            host: "192.0.2.1".to_string()
        }),
        "the refusal must name the hop it refused, got {err:?}"
    );
    assert_eq!(
        server.accepted(),
        1,
        "only the admitted first hop should ever have been connected to"
    );
    // The trap in reading "refused" as "nothing was sent". This refusal
    // happened *after* the first request was written and answered — the
    // accept count above is the proof — so the provider has it and may have
    // billed it. A retry here is the caller's decision to take knowingly.
    assert_eq!(
        err.delivery(),
        DeliveryCertainty::PossiblySent,
        "a redirect is refused after the original request was already sent"
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

// --- and what it says about whether the request bytes left ---

/// The three readings a wire format cannot take for itself, because by the
/// time a failure reaches it the evidence is prose.
///
/// A pre-flight refusal and a failed connect are `DefinitelyUnsent`: the
/// first decided before a socket existed, the second is the connector
/// reporting no handshake completed. (A *redirect* refusal is neither — see
/// `an_admitted_server_cannot_redirect_the_connection_off_the_machine`, where
/// the request had already been sent.) A body that stops arriving is
/// `PossiblySent` — the
/// provider has the request and may already have billed it, so a retry is the
/// caller's decision to take knowingly, not the transport's to take for them.
#[tokio::test]
async fn the_transport_says_whether_the_request_bytes_could_have_left() {
    let offline = ReqwestHttp::new(EgressPolicy::LoopbackOnly).expect("client builds");
    let refused = get(&offline, "https://api.openai.com/v1/chat/completions")
        .await
        .expect_err("a remote host is refused offline");
    assert_eq!(
        refused.delivery(),
        DeliveryCertainty::DefinitelyUnsent,
        "a refusal costs no packets, so a retry elsewhere cannot double-bill"
    );

    // A port that was free a moment ago and has nothing on it now. Binding
    // first is what makes it free rather than hopefully-free.
    let dead = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("binding a loopback port should succeed");
    let dead_port = dead.local_addr().expect("bound socket").port();
    drop(dead);
    let unreachable = get(&offline, &format!("http://127.0.0.1:{dead_port}/v1/models"))
        .await
        .expect_err("nothing is listening on that port");
    assert_eq!(
        unreachable.delivery(),
        DeliveryCertainty::DefinitelyUnsent,
        "the connector never completed a handshake, so no request line was written: {unreachable}"
    );

    // A response that promises a hundred bytes and delivers five, then hangs
    // up: exactly the shape of a provider that started answering and died.
    const TRUNCATED: &str = "HTTP/1.1 200 OK\r\n\
         content-type: text/event-stream\r\n\
         content-length: 100\r\n\
         \r\n\
         hello";
    let server = Listener::serving(TRUNCATED).await;
    let cut = get(
        &offline,
        &format!("http://127.0.0.1:{}/v1/models", server.port),
    )
    .await
    .expect_err("a body that ends early is an error, not a short read");
    assert_eq!(
        cut.delivery(),
        DeliveryCertainty::PossiblySent,
        "the provider answered, so it may already have billed the request: {cut}"
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

/// The checklist item is "no credentials or request bodies reachable from
/// `PublicEvent`", and `Secret`'s redaction does not reach this far.
///
/// `Secret` refuses to print in every formatter, and the plan records the
/// argument for putting the guarantee there: a value that refuses makes a
/// forgetful container harmless. That holds right up to
/// `ProviderRow::headers`, which calls `expose()` and hands back
/// `format!("Bearer {…}")` as an ordinary `String`. `HttpRequest` is what
/// receives it, so it is the container the argument does not cover — and it
/// carries the prompt as well.
///
/// Nothing formats an `HttpRequest` today, so this asserts no live leak. It
/// asserts that the obvious next line of transport debugging —
/// `tracing::debug!(?req)` — cannot become one.
#[test]
fn a_formatted_request_prints_neither_the_credential_nor_the_body() {
    let printed = format!(
        "{:?}",
        HttpRequest {
            url: Url::parse("https://api.example.com/v1/chat/completions").unwrap(),
            method: Method::Post,
            headers: vec![
                (
                    "authorization".to_string(),
                    "Bearer sk-live-SECRET".to_string()
                ),
                ("content-type".to_string(), "application/json".to_string()),
            ],
            body: Some(br#"{"messages":[{"role":"user","content":"PROMPT"}]}"#.to_vec()),
        }
    );

    assert!(
        !printed.contains("sk-live-SECRET") && !printed.contains("Bearer"),
        "a credential reached a formatter: {printed}"
    );
    assert!(
        !printed.contains("PROMPT") && !printed.contains("messages"),
        "a request body reached a formatter: {printed}"
    );
    // Diagnosability is the reason this is a redacting `Debug` and not a
    // deleted one: a reader still gets the destination, the method, which
    // headers were set, and how big the body was.
    assert!(printed.contains("api.example.com"), "{printed}");
    assert!(printed.contains("authorization"), "{printed}");
    assert!(printed.contains("content-type"), "{printed}");
    assert!(printed.contains("<49 bytes>"), "{printed}");
}

/// The other half of the same guarantee, and the half everything else assumes.
///
/// The test above covers the one container `Secret` does not reach. This
/// covers `Secret` itself, which had no test at all — and it is the claim the
/// rest of the design is built on. `ResolvedCredential` **derives** `Debug`
/// and its own doc says that is "safe because `Secret` refuses to print"; a
/// `ResolvedModel` is `Clone` and `Debug` and travels to the budget gate and
/// the ledger on the argument that the credential is not in it. Every one of
/// those sentences is true because of four lines of hand-written `fmt::Debug`
/// that a derive would silently replace.
///
/// Asserted through `resolve` rather than by constructing a `Secret`, because
/// that is the path a run takes: an environment variable, a strategy, and the
/// container a generator holds.
#[test]
fn a_resolved_credential_prints_no_part_of_the_credential() {
    let credential = CredentialStrategy::BearerFromEnv {
        var: "PROVIDER_KEY".to_string(),
    }
    .resolve(&Configured)
    .expect("the fake holds every variable");
    let printed = format!("{credential:?}");

    assert!(
        !printed.contains(LIVE_KEY),
        "a credential reached a formatter: {printed}"
    );
    // Not only the secret: the `Bearer ` the strategy prepended is part of the
    // header value, and a formatter that printed it would be printing the
    // start of the string the secret is the rest of.
    assert!(
        !printed.contains("Bearer"),
        "the credential's header value reached a formatter: {printed}"
    );
    // Redacting, not deleted — the same trade the request's `Debug` makes.
    // Which header was set is diagnosable; what it was set to is not.
    assert!(printed.contains("authorization"), "{printed}");
}

/// The value [`Configured`] answers with, so the test above has something to
/// look for that could not appear by chance.
const LIVE_KEY: &str = "sk-live-a-string-nothing-else-here-contains";

/// A [`Credentials`] with every variable set, to [`LIVE_KEY`].
struct Configured;

impl Credentials for Configured {
    fn secret(&self, _var: &str) -> Result<Secret, MissingCredential> {
        Ok(LIVE_KEY.into())
    }
}
