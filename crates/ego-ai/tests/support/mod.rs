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

use ego_ai::http::{Http, HttpError, HttpRequest, HttpResponse, Method};
use ego_ai::provider::{
    CredentialStrategy, Credentials, MissingCredential, ResolvedCredential, Secret,
};
use ego_core::generator::attempt::DeliveryCertainty;
use futures::future::BoxFuture;

/// The bearer value [`credential`] resolves to, so a test that inspects the
/// header it becomes has something to compare against.
pub const TEST_KEY: &str = "test-key";

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
    answers: Mutex<VecDeque<Answer>>,
    seen: Mutex<Vec<Asked>>,
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
            answers: Mutex::new(answers.into_iter().map(Into::into).collect()),
            seen: Mutex::new(Vec::new()),
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
    fn execute(&self, req: HttpRequest) -> BoxFuture<'static, Result<HttpResponse, HttpError>> {
        self.seen.lock().unwrap().push(Asked {
            method: req.method,
            path: req.url.path().to_string(),
            headers: req.headers.clone(),
            body: req
                .body
                .as_ref()
                .map(|body| String::from_utf8_lossy(body).to_string()),
        });
        let answer = self.answers.lock().unwrap().pop_front();
        Box::pin(async move {
            match answer {
                Some(answer) => Ok(HttpResponse {
                    status: answer.status,
                    headers: std::iter::once((
                        "content-type".to_string(),
                        "application/json".to_string(),
                    ))
                    .chain(answer.headers)
                    .collect(),
                    body: Box::pin(futures::stream::once(async move {
                        Ok(bytes::Bytes::from(answer.body.into_bytes()))
                    })),
                }),
                None => Err(HttpError::Transport {
                    message: "the stub was asked more than it was scripted for".to_string(),
                    delivery: DeliveryCertainty::DefinitelyUnsent,
                }),
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
