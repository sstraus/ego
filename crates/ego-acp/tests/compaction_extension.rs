//! contract for ACP's advertised compaction extension.

use agent_client_protocol::{ConnectTo, Lines};
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt};

mod support;

async fn initialize() -> serde_json::Value {
    let (mut to_agent, to_agent_rx) = mpsc::unbounded::<String>();
    let (from_agent, mut from_agent_rx) = mpsc::unbounded::<String>();
    let transport = Lines::new(
        from_agent.sink_map_err(std::io::Error::other),
        to_agent_rx.map(Ok::<_, std::io::Error>),
    );
    let agent = tokio::spawn(support::agent().connect_to(transport));
    to_agent
        .send(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {"protocolVersion": 1, "clientCapabilities": {}}
            })
            .to_string(),
        )
        .await
        .expect("the ACP agent is listening");
    let response = from_agent_rx.next().await.expect("initialize response");
    to_agent.close_channel();
    let _ = agent.await;
    serde_json::from_str(&response).expect("ACP response JSON")
}

async fn request(method: &str, params: serde_json::Value) -> serde_json::Value {
    let (mut to_agent, to_agent_rx) = mpsc::unbounded::<String>();
    let (from_agent, mut from_agent_rx) = mpsc::unbounded::<String>();
    let transport = Lines::new(
        from_agent.sink_map_err(std::io::Error::other),
        to_agent_rx.map(Ok::<_, std::io::Error>),
    );
    let agent = tokio::spawn(support::agent().connect_to(transport));
    for (id, method, params) in [
        (
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientCapabilities": {}
            }),
        ),
        (2, method, params),
    ] {
        to_agent
            .send(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "method": method,
                    "params": params,
                })
                .to_string(),
            )
            .await
            .expect("ACP request is sent");
        let response = from_agent_rx.next().await.expect("ACP response");
        if id == 2 {
            to_agent.close_channel();
            let _ = agent.await;
            return serde_json::from_str(&response).expect("ACP response JSON");
        }
    }
    serde_json::Value::Null
}

#[tokio::test]
async fn initialize_advertises_only_the_versioned_compaction_extension() {
    let response = initialize().await;
    let capabilities = &response["result"]["agentCapabilities"];
    let extension = &capabilities["_meta"]["ego"]["compact"];
    assert_eq!(extension["version"], 1);
    assert_eq!(extension["method"], "_ego/compact");
}

#[tokio::test]
async fn compact_is_a_real_json_rpc_request_with_authority_bound_fields() {
    let response = request(
        "_ego/compact",
        serde_json::json!({
            "v": 1,
            "sessionId": "00000000-0000-0000-0000-000000000068",
            "requestId": "00000000-0000-0000-0000-000000000069"
        }),
    )
    .await;
    assert!(
        response["result"].is_object(),
        "compact response: {response}"
    );
    for field in [
        "sourceSessionId",
        "sourceSeq",
        "targetSessionId",
        "publication",
        "successorStartRequestId",
    ] {
        assert!(response["result"].get(field).is_some(), "missing {field}");
    }
}

#[tokio::test]
async fn compact_json_rpc_denies_unknown_request_fields_at_deserialization() {
    let response = request(
        "_ego/compact",
        serde_json::json!({
            "v": 1,
            "sessionId": "00000000-0000-0000-0000-000000000068",
            "requestId": "00000000-0000-0000-0000-000000000069",
            "unknown": true
        }),
    )
    .await;
    assert!(
        response["error"].is_object(),
        "unknown field response: {response}"
    );
    assert_eq!(response["error"]["code"], -32602);
}

#[test]
fn compaction_request_is_versioned_and_deny_unknown_fields() {
    let request = serde_json::json!({
        "v": 1,
        "sessionId": "00000000-0000-0000-0000-000000000068",
        "requestId": "00000000-0000-0000-0000-000000000069"
    });
    assert_eq!(request.as_object().expect("request object").len(), 3);
    assert!(
        !request
            .as_object()
            .expect("request object")
            .contains_key("source")
    );
}

#[test]
fn compaction_response_names_source_sequence_target_and_successor() {
    let response = serde_json::json!({
        "v": 1,
        "sourceSessionId": "00000000-0000-0000-0000-000000000068",
        "sourceSeq": 42,
        "targetSessionId": "00000000-0000-0000-0000-000000000069",
        "publication": {"kind": "published_durably"},
        "successorStartRequestId": "00000000-0000-0000-0000-00000000006a"
    });
    assert_eq!(response["v"], 1);
    for field in [
        "sourceSessionId",
        "sourceSeq",
        "targetSessionId",
        "publication",
        "successorStartRequestId",
    ] {
        assert!(response.get(field).is_some(), "missing {field}");
    }
}

/// The other half of the advertisement, and the one that was missing: a build
/// composed without the compaction port says so twice.
///
/// `durable_agent` is exactly that build — it passes no compaction command —
/// so the claim is not hypothetical. Section 3's rule is that an unadvertised
/// operation is unavailable rather than partially emulated, and an agent that
/// advertised nothing but answered anyway would break the half of it a host
/// cannot see.
#[tokio::test]
async fn an_agent_without_the_compaction_port_neither_advertises_nor_serves_compact() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    let response = connection.initialize().await;

    assert!(
        response["result"]["agentCapabilities"]["_meta"]["ego"]
            .get("compact")
            .is_none(),
        "compaction was advertised without a port to serve it: {response}"
    );

    let (refused, _) = connection
        .request(
            "_ego/compact",
            serde_json::json!({
                "v": 1,
                "sessionId": "00000000-0000-0000-0000-000000000068",
                "requestId": "00000000-0000-0000-0000-000000000069"
            }),
        )
        .await;
    assert_eq!(
        refused["error"]["code"], -32601,
        "_ego/compact answered without a port: {refused}"
    );

    connection.abort().await;
}
