//! What the test binaries in this directory share.
//!
//! One file rather than a copy per binary: `Credentials` is the seam every
//! test that assembles a request has to stand in for, and two doubles that
//! drifted apart would let one binary prove something the other disproved.

// Each integration test is its own crate and compiles this module separately,
// so an item only one binary needs is dead code in the others. The alternative
// is an API contorted to be fully used everywhere, which is a worse trade than
// one scoped allow on a test-only module.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use ego_ai::http::{Http, HttpCall, Method, PreparedHttpCall};
use ego_ai::provider::{
    CredentialStrategy, Credentials, MissingCredential, ResolvedCredential, Secret,
};
use ego_core::permit::{
    DnsResolution, DuplexHandle, DuplexSpawner, HttpHeader, NetworkTarget, OneHop, OneHopResponse,
    ResolvedConnect, Resolver, SpawnSpec, TransportError, TransportFailure,
};
use futures::future::BoxFuture;

/// The bearer value [`credential`] resolves to, so a test that inspects the
/// header it becomes has something to compare against.
pub const TEST_KEY: &str = "test-key";

/// Inert foreign-effect ports for tests that exercise unrelated turn paths.
/// Each effect fails before any external work, so a test cannot accidentally
/// reach a real network or child process while assembling a turn.
pub struct NoForeign;

impl Resolver for NoForeign {
    fn resolve(
        &self,
        _target: NetworkTarget,
        _deadline: Option<Instant>,
    ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
        Box::pin(futures::future::ready(Err(
            TransportError::definitely_unsent(TransportFailure::AuthorityMismatch),
        )))
    }
}

impl OneHop for NoForeign {
    fn execute(
        &self,
        _connect: ResolvedConnect,
        _request: ego_core::permit::HttpRequest,
        _deadline: Option<Instant>,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        Box::pin(futures::future::ready(Err(
            TransportError::definitely_unsent(TransportFailure::AuthorityMismatch),
        )))
    }
}

impl DuplexSpawner for NoForeign {
    fn spawn(&self, _spec: SpawnSpec) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
        Box::pin(futures::future::ready(Err(
            TransportError::definitely_unsent(TransportFailure::AuthorityMismatch),
        )))
    }
}

/// A resolved credential, for a test whose subject is not the credential.
///
/// Every resolution now takes one — `Catalogue::resolve` passes it to whatever
/// the row names, because a resolver whose endpoint authenticates must send the
/// credential this run already read rather than read the environment again. Most
/// tests do not care which one; this is what they pass.
pub fn credential() -> ResolvedCredential {
    CredentialStrategy::BearerFromEnv {
        var: "EGO_TEST_KEY".to_string(),
    }
    .resolve(&FakeCredentials::holding(TEST_KEY))
    .expect("the fake holds every variable")
}

/// A [`Credentials`] that answers with what it was built with, whatever
/// variable is asked for.
///
/// The variable name is not matched on purpose. A test double that checked it
/// would be re-implementing the lookup [`EnvCredentials`](ego_ai::provider::EnvCredentials)
/// owns, and the thing under test here is never *which* variable was consulted
/// — it is what the row does with the answer.
pub struct FakeCredentials(Result<&'static str, ()>);

impl FakeCredentials {
    /// Every variable is set, to `value`.
    pub fn holding(value: &'static str) -> Self {
        FakeCredentials(Ok(value))
    }

    /// No variable is set.
    pub fn empty() -> Self {
        FakeCredentials(Err(()))
    }
}

impl Credentials for FakeCredentials {
    fn secret(&self, var: &str) -> Result<Secret, MissingCredential> {
        match self.0 {
            Ok(value) => Ok(value.into()),
            Err(()) => Err(MissingCredential {
                var: var.to_string(),
            }),
        }
    }
}

/// One request a [`StubServer`] was asked to make.
#[derive(Debug, Clone)]
pub struct Asked {
    pub method: Method,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

/// A transport that answers a scripted queue and records what it was asked.
///
/// Whole-body rather than streaming, which is what every *control-plane*
/// endpoint in this crate needs: Ollama's `/api/tags` and `/api/ps`, and xAI's
/// `/language-models`. The three stubs that stream SSE are a different shape
/// and stay where they are; this exists so the two binaries that ask a JSON
/// endpoint a question do not each grow their own.
///
/// A queue and not a router. A test that scripts two answers is asserting the
/// *order* the resolver asks in as well as what it does with the replies, and
/// a map keyed by path would silently pass a resolver that asked backwards.
pub struct StubServer {
    answers: Arc<Mutex<VecDeque<Answer>>>,
    seen: Arc<Mutex<Vec<Asked>>>,
}

/// One scripted reply.
///
/// A struct rather than the `(status, body)` pair it grew from, because a
/// caching fetch is scripted by its *headers* — an `ETag` is what a `304` is
/// about — and a pair cannot carry one. The `From` below keeps every existing
/// call site spelled as a pair, since a test that does not care about headers
/// should not have to say so.
pub struct Answer {
    pub status: u16,
    pub body: String,
    /// Beside the `content-type` every answer carries.
    pub headers: Vec<(String, String)>,
}

impl From<(u16, &'static str)> for Answer {
    fn from((status, body): (u16, &'static str)) -> Self {
        Answer {
            status,
            body: body.to_string(),
            headers: Vec::new(),
        }
    }
}

impl Answer {
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

impl StubServer {
    pub fn new(answers: impl IntoIterator<Item = impl Into<Answer>>) -> Arc<Self> {
        Arc::new(StubServer {
            answers: Arc::new(Mutex::new(answers.into_iter().map(Into::into).collect())),
            seen: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Everything it was asked, in order.
    pub fn asked(&self) -> Vec<Asked> {
        self.seen.lock().unwrap().clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.asked().into_iter().map(|ask| ask.path).collect()
    }

    pub fn bodies(&self) -> Vec<Option<String>> {
        self.asked().into_iter().map(|ask| ask.body).collect()
    }
}

impl Http for StubServer {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(StubCall {
            answers: Arc::clone(&self.answers),
            seen: Arc::clone(&self.seen),
        })
    }
}

struct StubCall {
    answers: Arc<Mutex<VecDeque<Answer>>>,
    seen: Arc<Mutex<Vec<Asked>>>,
}

impl HttpCall for StubCall {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let (_, _route, _, request) = call.into_parts();
        let method = match request.method() {
            "GET" => Method::Get,
            "POST" => Method::Post,
            other => panic!("unsupported fixture method {other}"),
        };
        let headers = request
            .headers()
            .iter()
            .map(|header| {
                (
                    header.name().to_owned(),
                    String::from_utf8(header.value().to_vec()).expect("fixture header is UTF-8"),
                )
            })
            .collect();
        self.seen.lock().unwrap().push(Asked {
            method,
            path: request.path().to_owned(),
            headers,
            body: (!request.body().is_empty()).then(|| {
                String::from_utf8(request.body().to_vec()).expect("fixture body is UTF-8")
            }),
        });
        let answer = self.answers.lock().unwrap().pop_front();
        Box::pin(async move {
            match answer {
                Some(answer) => {
                    let headers = std::iter::once(
                        HttpHeader::new("content-type", b"application/json".to_vec())
                            .expect("fixture header"),
                    )
                    .chain(answer.headers.into_iter().map(|(name, value)| {
                        HttpHeader::new(&name, value.into_bytes()).expect("fixture header")
                    }))
                    .collect();
                    Ok(OneHopResponse::response(
                        answer.status,
                        headers,
                        answer.body.into_bytes(),
                    ))
                }
                None => Err(TransportError::definitely_unsent(
                    TransportFailure::Exchange(
                        "the stub was asked more than it was scripted for".to_string(),
                    ),
                )),
            }
        })
    }
}

/// A scripted server, as the two things a caller needs: the port, and the
/// record.
/// A server scripted with nothing, for a test whose claim is that nothing is
/// asked.
///
/// Named rather than spelled `server([])`, which no longer infers now that an
/// answer may be written as a pair *or* as an [`Answer`] — and which read as
/// "an empty script" where what the caller means is "any request at all is the
/// failure". The stub errors on an unscripted request, so that is exactly what
/// this asserts.
pub fn unasked() -> (Arc<dyn Http>, Arc<StubServer>) {
    server(Vec::<Answer>::new())
}

pub fn server(
    answers: impl IntoIterator<Item = impl Into<Answer>>,
) -> (Arc<dyn Http>, Arc<StubServer>) {
    let stub = StubServer::new(answers);
    (Arc::clone(&stub) as Arc<dyn Http>, stub)
}
