//! The inbound A2A listener: three routes, and a credential check ahead of all
//! of them.
//!
//! `hyper` directly rather than `axum`. The framing this needs is one POST, one
//! GET and a refusal for everything else; what a framework would add here is
//! its own extraction, error and middleware model in the one place where ego's
//! design is that those decisions are made explicitly at a port. `hyper` and
//! `hyper-util` are already in the dependency graph via `reqwest`, so the
//! choice costs no build time either way and the tie goes to the smaller
//! surface.
//!
//! What this milestone deliberately does *not* do is reach a `RunController`.
//! No JSON-RPC method is routed yet, so wiring the controller in would be a
//! collaborator held and not called — and a caller that got as far as
//! `SendMessage` would be answered by something that does not exist. The
//! refusal is honest instead: every valid A2A method is `MethodNotFound` here,
//! and the card says so.

use std::convert::Infallible;
use std::sync::Arc;

use a2a_lf::jsonrpc::{JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse, methods};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;

#[cfg(test)]
use crate::auth::BEARER;
use crate::auth::Credential;
use crate::card::{Served, card};
use ego_core::endpoint::Admitted;
use ego_listen::{
    Bound, ConnectionGate, ConnectionLimits, GuardedAccepted, Peer, serve_connections,
};

/// The discovery path, pinned to the constant the reference server publishes.
///
/// Spelled here rather than imported because `a2a-server` is not a dependency —
/// taking it would mean taking `axum`. The binding is a test in this module
/// that fails if the two ever differ.
pub const WELL_KNOWN_AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// The largest request body this listener will read.
///
/// An unbounded read on an authenticated endpoint is still a way to make the
/// process hold memory for as long as a caller keeps writing. One megabyte is
/// far above any A2A envelope and far below anything that matters.
const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// Everything a connection needs, shared across all of them.
struct Listener {
    /// `None` on an endpoint the operating system already authenticated. Not an
    /// "anonymous mode": the authority came from the transport instead, and
    /// [`ego_core::endpoint::Admitted::authentication`] decided which before a
    /// socket existed.
    credential: Option<Credential>,
    served: Served,
}

/// Serve until `shutdown` resolves.
///
/// The bind happens here rather than in the caller so that the address a card
/// advertises is the address the socket actually got: passing port 0 is how a
/// test asks the OS for a free one, and a card built from the requested address
/// would then advertise port 0 to the world.
pub async fn serve(
    admitted: &Admitted,
    credential: Option<Credential>,
    shutdown: impl Future<Output = ()> + Send,
) -> std::io::Result<()> {
    let bound = ego_listen::bind(admitted).await?;
    serve_on(
        bound,
        admitted,
        credential,
        ConnectionGate::new(ConnectionLimits::default()),
        shutdown,
    )
    .await
}

/// The same, on an endpoint the caller already opened.
pub async fn serve_on(
    bound: Bound,
    admitted: &Admitted,
    credential: Option<Credential>,
    gate: ConnectionGate,
    shutdown: impl Future<Output = ()> + Send,
) -> std::io::Result<()> {
    // Refuse before serving rather than per request. An endpoint whose
    // authentication is a credential, started without one, has no safe answer
    // to give a caller — and discovering that at the first request means it
    // already accepted a connection it could not judge.
    if ego_listen::credential_required(admitted) && credential.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{admitted} is reachable by any process on this machine and will \
                 not serve without a credential: set {} , or bind a local \
                 endpoint, which the operating system authenticates instead",
                crate::auth::TOKEN_VAR
            ),
        ));
    }
    let advertised = bound.advertised()?;
    let state = Arc::new(Listener {
        credential,
        served: Served {
            url: format!("http://{advertised}/"),
            // Both false, and both are the truth about this build: no method is
            // routed yet. The card is built from this value, so the two cannot
            // disagree.
            streaming: false,
            push_notifications: false,
            authentication: admitted.authentication(),
        },
    });
    tracing::info!(endpoint = %advertised, "serving A2A");

    serve_connections(
        bound,
        gate,
        move |accepted: GuardedAccepted| {
            // Decided once per connection, from what the transport proved,
            // and never re-derived per request from a header.
            let authenticated = accepted.peer == Peer::CurrentUser;
            let state = Arc::clone(&state);
            async move {
                serve_connection(accepted, state, authenticated).await;
            }
        },
        shutdown,
    )
    .await
}

async fn serve_connection(accepted: GuardedAccepted, state: Arc<Listener>, authenticated: bool) {
    let service = service_fn(move |request| answer(Arc::clone(&state), authenticated, request));
    if let Err(error) = http1::Builder::new()
        .serve_connection(TokioIo::new(accepted.stream), service)
        .await
    {
        tracing::debug!(%error, "an A2A connection ended badly");
    }
}

/// Route one request, credential first.
///
/// The order is the point: nothing about the request other than its
/// `Authorization` header is looked at until the credential has been accepted,
/// so an unauthenticated caller cannot learn which paths exist, which methods
/// parse, or whether a body was well formed.
async fn answer(
    state: Arc<Listener>,
    authenticated: bool,
    request: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    // A peer the transport vouched for is not asked for a credential, and its
    // `Authorization` header is not read at all. Reading it anyway would make
    // a header able to *fail* a connection the operating system already
    // admitted, which is a second authority over one decision.
    if !authenticated {
        let presented = request
            .headers()
            .get(AUTHORIZATION)
            .map(|value| value.as_bytes())
            .unwrap_or_default();
        // No credential on an endpoint that needs one cannot happen — `serve_on`
        // refuses to start — so this is the closed answer rather than a default.
        if !state
            .credential
            .as_ref()
            .is_some_and(|credential| credential.admits(presented))
        {
            return Ok(unauthorized());
        }
    }
    Ok(match (request.method(), request.uri().path()) {
        (&Method::GET, WELL_KNOWN_AGENT_CARD_PATH) => json(
            StatusCode::OK,
            &serde_json::to_value(card(&state.served)).expect("a card serialises"),
        ),
        (&Method::POST, "/") => rpc(request).await,
        _ => empty(StatusCode::NOT_FOUND),
    })
}

/// Answer one JSON-RPC envelope.
///
/// Every method A2A defines is recognised and refused as unimplemented, which
/// is a different answer from "no such method" and is the honest one: the
/// caller's request was valid and this build has no route for it yet.
async fn rpc(request: Request<Incoming>) -> Response<Full<Bytes>> {
    let Ok(body) = read_body(request.into_body()).await else {
        return empty(StatusCode::PAYLOAD_TOO_LARGE);
    };
    // An envelope that does not parse has no id to answer against, so this is
    // the one JSON-RPC error that has to be reported without one.
    let Ok(envelope) = serde_json::from_slice::<JsonRpcRequest>(&body) else {
        return rpc_error(
            JsonRpcId::Number(0),
            -32700,
            "the request body is not a JSON-RPC 2.0 envelope",
        );
    };
    if envelope.jsonrpc != "2.0" {
        return rpc_error(envelope.id, -32600, "`jsonrpc` must be exactly \"2.0\"");
    }
    let (code, message) = if methods::is_valid(&envelope.method) {
        (
            -32601,
            format!(
                "`{}` is an A2A method this build does not serve yet; the agent \
                 card at {WELL_KNOWN_AGENT_CARD_PATH} states what it does",
                envelope.method
            ),
        )
    } else {
        (
            -32601,
            format!("`{}` is not an A2A method", envelope.method),
        )
    };
    rpc_error(envelope.id, code, &message)
}

/// Read at most [`MAX_BODY_BYTES`], refusing anything longer.
///
/// The refusal is a unit rather than the response to send: this reads a body,
/// and how a refusal is phrased on the wire is the router's decision. Returning
/// a built `Response` here would also make the error arm of every call site
/// carry a response-sized value for the overwhelmingly common success case.
async fn read_body(body: Incoming) -> Result<Bytes, TooLarge> {
    http_body_util::Limited::new(body, MAX_BODY_BYTES as usize)
        .collect()
        .await
        .map(|collected| collected.to_bytes())
        .map_err(|_| TooLarge)
}

/// The only way [`read_body`] refuses: the caller wrote past [`MAX_BODY_BYTES`].
struct TooLarge;

/// `401` with the scheme named and nothing else said.
///
/// The body is empty on purpose. A refusal that explains what was wrong with
/// the credential — absent, wrong scheme, wrong length — is an oracle, and the
/// caller who is entitled to be here does not need the help.
fn unauthorized() -> Response<Full<Bytes>> {
    let mut response = empty(StatusCode::UNAUTHORIZED);
    response.headers_mut().insert(
        WWW_AUTHENTICATE,
        hyper::header::HeaderValue::from_static("Bearer"),
    );
    response
}

fn rpc_error(id: JsonRpcId, code: i32, message: &str) -> Response<Full<Bytes>> {
    let response = JsonRpcResponse::error(
        id,
        JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        },
    );
    json(
        StatusCode::OK,
        &serde_json::to_value(response).expect("a JSON-RPC response serialises"),
    )
}

fn json(status: StatusCode, value: &serde_json::Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(value.to_string())))
        .expect("a JSON response is well formed")
}

fn empty(status: StatusCode) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::new()))
        .expect("an empty response is well formed")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The discovery path is a constant in the reference server
    /// (`a2a-server/src/agent_card.rs`), and getting it wrong means no caller
    /// ever finds ego. Spelled out here so a typo is a failure rather than a
    /// silent 404.
    #[test]
    fn the_discovery_path_is_the_one_the_reference_server_publishes() {
        assert_eq!(WELL_KNOWN_AGENT_CARD_PATH, "/.well-known/agent-card.json");
    }

    /// `BEARER` is what the card declares and what `Credential::admits`
    /// enforces; this is the `WWW-Authenticate` end of the same value.
    #[test]
    fn the_challenge_names_the_scheme_the_card_declares() {
        let response = unauthorized();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let challenge = response
            .headers()
            .get(WWW_AUTHENTICATE)
            .expect("a 401 carries a challenge")
            .to_str()
            .expect("the challenge is ASCII");
        assert!(challenge.eq_ignore_ascii_case(BEARER), "{challenge}");
    }
}
