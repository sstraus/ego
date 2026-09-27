//! ACP wire settlements for cancellation, permission refusal, and durable holds.

mod support;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/acp-settlement");
const REQUEST: &str = "00000000-0000-0000-0000-000000000153";

async fn opened(connection: &mut support::Connected) -> String {
    connection.initialize().await;
    connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned()
}

fn prompt(session: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionId": session,
        "prompt": [{"type": "text", "text": "Run the command"}]
    })
}

#[tokio::test]
async fn cancel_during_permission_answers_cancelled() {
    let fixture = support::DurableAcpFixture::new();
    fixture.ask_permission_next_turn();
    let mut connection = support::Connected::new(fixture.router());
    let session = opened(&mut connection).await;
    let pending = connection
        .send_request("session/prompt", prompt(&session))
        .await;
    let permission = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.agent_request(),
    )
    .await
    .expect("permission request");
    assert_eq!(permission["method"], "session/request_permission");
    connection
        .notify("session/cancel", serde_json::json!({"sessionId": session}))
        .await;
    connection
        .answer(
            &permission,
            serde_json::json!({"outcome": {"outcome": "cancelled"}}),
        )
        .await;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.response(pending),
    )
    .await
    .expect("prompt settles")
    .0;
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    connection.abort().await;
}

#[tokio::test]
async fn reject_permission_without_cancel_remains_a_single_refusal() {
    let fixture = support::DurableAcpFixture::new();
    fixture.ask_permission_next_turn();
    let mut connection = support::Connected::new(fixture.router());
    let session = opened(&mut connection).await;
    let pending = connection
        .send_request("session/prompt", prompt(&session))
        .await;
    let permission = connection.agent_request().await;
    assert_eq!(permission["method"], "session/request_permission");
    connection
        .answer(
            &permission,
            serde_json::json!({"outcome": {"outcome": "selected", "optionId": "reject-once"}}),
        )
        .await;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.response(pending),
    )
    .await
    .expect("prompt settles")
    .0;
    assert_eq!(answer["error"]["code"], -32603, "{answer}");
    let message = answer["error"]["message"]
        .as_str()
        .expect("refusal message");
    assert_eq!(
        message.matches("permission was refused").count(),
        1,
        "{answer}"
    );
    connection.abort().await;
}

#[tokio::test]
async fn held_session_is_discoverable_on_list_and_load_and_prompt_has_a_dedicated_code() {
    let fixture = support::DurableAcpFixture::new();
    let mut first = support::Connected::new(fixture.router());
    let session = opened(&mut first).await;
    let (paused, _) = first
        .request(
            "_ego/pause",
            serde_json::json!({
                "v": 1, "sessionId": session, "requestId": REQUEST
            }),
        )
        .await;
    assert_eq!(paused["result"]["state"], "paused", "{paused}");
    first.abort().await;

    let mut reconnect = support::Connected::new(fixture.router());
    reconnect.initialize().await;
    let (listed, _) = reconnect
        .request("session/list", serde_json::json!({"cwd": ROOT}))
        .await;
    let row = listed["result"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["sessionId"] == session)
        .expect("held session listed");
    assert_eq!(row["_meta"]["ego"]["hold"]["state"], "paused", "{listed}");
    let (loaded, _) = reconnect
        .request(
            "session/load",
            serde_json::json!({
                "sessionId": session, "cwd": ROOT, "mcpServers": []
            }),
        )
        .await;
    assert_eq!(
        loaded["result"]["_meta"]["ego"]["hold"]["state"], "paused",
        "{loaded}"
    );
    let (refused, _) = reconnect.request("session/prompt", prompt(&session)).await;
    assert_eq!(refused["error"]["code"], -32011, "{refused}");
    let (resumed, _) = reconnect
        .request(
            "_ego/resume",
            serde_json::json!({
                "v": 1, "sessionId": session, "requestId": "00000000-0000-0000-0000-000000000154"
            }),
        )
        .await;
    assert_eq!(resumed["result"]["state"], "running", "{resumed}");
    let (listed, _) = reconnect
        .request("session/list", serde_json::json!({"cwd": ROOT}))
        .await;
    let row = listed["result"]["sessions"]
        .as_array()
        .expect("session rows")
        .iter()
        .find(|row| row["sessionId"] == session)
        .expect("resumed session listed");
    assert_eq!(row["_meta"]["ego"]["hold"]["state"], "running", "{listed}");
    reconnect.abort().await;
}
