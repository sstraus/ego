//! What the agent answers a real `initialize` with, over a real connection.
//!
//! `tests/negotiate.rs` reads the rule as a function. This drives it through
//! the SDK's own router and asks what came back on the wire, because since ADR
//! §6 the negotiation is the SDK's: our decision is which arms
//! [`ego_acp::serve::agent`] configures, and only the router can say what that
//! configuration answers.
//!
//! Without this pair, `negotiate` would be a table this crate believes and
//! nothing checks — a binding whose two ends are both our own code. Here one
//! end is a dependency we do not control.

use agent_client_protocol::{ConnectTo, Lines};
use agent_client_protocol_schema::ProtocolVersion;
use ego_acp::negotiate::negotiate;
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};

mod support;

/// The JSON-RPC response to one `initialize` asking for `wanted`.
async fn initialize(wanted: u16) -> serde_json::Value {
    let (mut to_agent, to_agent_rx) = mpsc::unbounded::<String>();
    let (from_agent, mut from_agent_rx) = mpsc::unbounded::<String>();

    let transport = Lines::new(
        from_agent.sink_map_err(std::io::Error::other),
        to_agent_rx.map(Ok::<_, std::io::Error>),
    );
    let agent = tokio::spawn(support::agent().connect_to(transport));

    // The *client's* shape, not ours: a client asking for `2` or newer sends
    // v2's `initialize`, whose `info` is required where v1's `clientInfo` is
    // optional. Sending v1's shape under a v2 version number would fail on the
    // params before the router ever chose an arm, and this test would then be
    // asserting that a malformed request is refused — true, and not the
    // question.
    let params = if wanted >= ProtocolVersion::V2.as_u16() {
        serde_json::json!({
            "protocolVersion": wanted,
            "info": { "name": "a test", "version": "0" },
            "capabilities": {},
        })
    } else {
        serde_json::json!({ "protocolVersion": wanted, "clientCapabilities": {} })
    };
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": params,
    });
    to_agent
        .send(request.to_string())
        .await
        .expect("the agent is listening");

    let line = from_agent_rx
        .next()
        .await
        .expect("an initialize is always answered, with a result or an error");

    // Closing our end ends the connection, so the spawned agent finishes rather
    // than outliving the test and holding a runtime worker.
    to_agent.close_channel();
    let _ = agent.await;

    serde_json::from_str(&line).expect("the agent writes JSON-RPC")
}

/// The answered `protocolVersion`, or `None` when the response is an error.
///
/// Deliberately the same shape as [`negotiate`]'s return type: the whole
/// assertion below is that these two are the same function.
fn answered(response: &serde_json::Value) -> Option<ProtocolVersion> {
    let version = response.get("result")?.get("protocolVersion")?;
    serde_json::from_value(version.clone()).ok()
}

/// The two rows of ADR §6.3, asked of the router itself.
///
/// `0` is the row that corrects §3.2, and it is asserted here rather than only
/// in `negotiate.rs` because §3.2's claim was about what a *connection* does —
/// a pure function could have gone on agreeing with the ADR forever while the
/// wire said something else.
#[tokio::test]
async fn the_router_answers_what_negotiate_says_it_answers() {
    for wanted in [0u16, 1, 2, 3, 65535] {
        let response = initialize(wanted).await;
        assert_eq!(
            answered(&response),
            negotiate(ProtocolVersion::from(wanted)),
            "asking for {wanted} produced {response}, which `negotiate` does \
             not predict — the table and the connection have come apart"
        );
    }
}

/// A refusal is a JSON-RPC error, and it says what this endpoint does speak.
///
/// The complement of the test above: agreeing that `0` produces no
/// `protocolVersion` would also be satisfied by a hang, an empty object or a
/// crash. What a client receives has to be an error it can act on.
#[tokio::test]
async fn a_version_below_ours_is_refused_with_a_sentence_naming_ours() {
    let response = initialize(0).await;
    let error = response
        .get("error")
        .expect("version 0 is refused, so the response carries an error");
    let data = error
        .get("data")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        data.contains('1'),
        "the refusal does not tell the client which version to ask for: {response}"
    );
}

/// The capabilities answered are the ones this build has: Streamable HTTP MCP
/// ACP MCP and image prompts are supported, while SSE, audio, embedded
/// context and session loading are not.
///
/// A capability advertised is a request invited, so this pins that every
/// response field says what the implementation can actually honor rather
/// than defaulting to something a client would then rely on — `loadSession`
/// in particular, because a client that believes it can replay a session and
/// cannot is a client showing an empty transcript for a session that has one.
///
/// The converse matters just as much for `image`: this build reads image
/// blocks, and a client that is not told so has no way to offer the user an
/// attach button. Silence about a capability held is as wrong as noise about
/// one that is not.
#[tokio::test]
async fn nothing_is_advertised_that_this_build_cannot_do() {
    let response = initialize(1).await;
    let result = response.get("result").expect("version 1 is served");
    let capabilities = result
        .get("agentCapabilities")
        .expect("an initialize response states the agent's capabilities");

    assert_eq!(
        capabilities["promptCapabilities"]["image"],
        serde_json::json!(true),
        "image prompts are served but not advertised: {response}"
    );

    for claimed in ["loadSession", "audio", "embeddedContext"] {
        let advertised = capabilities
            .get(claimed)
            .or_else(|| capabilities.get("promptCapabilities")?.get(claimed))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        assert!(
            !advertised,
            "`{claimed}` is advertised and this build does not have it: {response}"
        );
    }

    let mcp = capabilities
        .get("mcpCapabilities")
        .and_then(serde_json::Value::as_object)
        .expect("an initialize response states MCP capabilities");
    assert_eq!(mcp.get("http"), Some(&serde_json::json!(true)));
    assert_eq!(mcp.get("sse"), Some(&serde_json::json!(false)));
    assert_eq!(mcp.get("acp"), Some(&serde_json::json!(true)));
}
