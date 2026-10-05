//! Uncertain runs must be abandonable without claiming their effects were resolved.

mod support;

use ego_core::run::RunStatus;
use serde_json::{Value, json};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/uncertain-session");

fn prompt(session: &str) -> Value {
    json!({"sessionId": session, "prompt": [{"type": "text", "text": "hello"}]})
}

async fn uncertain_session(
    fixture: &support::DurableAcpFixture,
    connection: &mut support::Connected,
) -> String {
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("new session")
        .to_owned();
    // Attachment failure makes the real controller publish an Uncertain settlement.
    fixture.fail_next_attach("effect authority could not be attached");
    let (failed, _) = connection.request("session/prompt", prompt(&session)).await;
    assert!(failed["error"].is_object(), "{failed}");
    assert_eq!(fixture.run_status(&session), RunStatus::Uncertain);
    session
}

// Catches: EffectUncertain blocks close after reconnect and strands the host in a dead chat.
#[tokio::test]
async fn uncertain_session_can_be_closed_after_reconnect_and_replaced() {
    let fixture = support::DurableAcpFixture::new();
    let mut first = support::Connected::new(fixture.router());
    let session = uncertain_session(&fixture, &mut first).await;
    first.abort().await;

    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let (closed, _) = connection
        .request("session/close", json!({"sessionId": session}))
        .await;
    assert!(closed["result"].is_object(), "{closed}");
    // Abandonment preserves the evidence; it does not relabel the uncertain effect.
    assert_eq!(fixture.run_status(&session), RunStatus::Uncertain);
    connection.abort().await;

    let mut reconnect = support::Connected::new(fixture.router());
    reconnect.initialize().await;
    let (listed, _) = reconnect
        .request("session/list", json!({"cwd": ROOT}))
        .await;
    assert!(
        listed["result"]["sessions"].as_array().unwrap().is_empty(),
        "{listed}"
    );
    let (loaded, _) = reconnect
        .request(
            "session/load",
            json!({"sessionId": session, "cwd": ROOT, "mcpServers": []}),
        )
        .await;
    assert_eq!(loaded["error"]["code"], -32002, "{loaded}");
    let new_session = reconnect.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("replacement session")
        .to_owned();
    assert_ne!(new_session, session);
    let (answer, _) = reconnect
        .request("session/prompt", prompt(&new_session))
        .await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    reconnect.abort().await;
}

// Catches: an Uncertain prompt returns opaque SESSION_BUSY or loses its diagnosis after failed load.
#[tokio::test]
async fn uncertain_session_prompt_names_run_and_close_new_chat_recovery() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    let session = uncertain_session(&fixture, &mut connection).await;
    let effects = fixture.executor_effect_counts();
    for reconnected in [false, true] {
        if reconnected {
            connection.abort().await;
            connection = support::Connected::new(fixture.router());
            connection.initialize().await;
            let (loaded, _) = connection
                .request(
                    "session/load",
                    json!({"sessionId": session, "cwd": ROOT, "mcpServers": []}),
                )
                .await;
            assert_eq!(loaded["error"]["code"], -32603, "{loaded}");
            assert!(
                loaded["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("close"),
                "{loaded}"
            );
        }
        let (answer, _) = connection.request("session/prompt", prompt(&session)).await;
        assert_eq!(answer["error"]["code"], -32603, "{answer}");
        let message = answer["error"]["message"].as_str().expect("visible error");
        assert!(message.contains(&format!("run {session}.")), "{answer}");
        assert!(
            message.contains("uncertain")
                && message.contains("session/close")
                && message.contains("new chat"),
            "{answer}"
        );
        assert_eq!(fixture.executor_effect_counts(), effects);
    }
    connection.abort().await;
}

// Catches: close reports success when durable abandonment failed and the session remains visible.
#[tokio::test]
async fn uncertain_session_close_reports_quarantine_failure_without_hiding_evidence() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    let session = uncertain_session(&fixture, &mut connection).await;
    fixture.fail_quarantine();
    let (closed, _) = connection
        .request("session/close", json!({"sessionId": session}))
        .await;
    assert_eq!(closed["error"]["code"], -32603, "{closed}");
    assert!(
        closed["error"]["data"]
            .as_str()
            .unwrap()
            .contains("quarantine marker write failure"),
        "{closed}"
    );
    let (listed, _) = connection
        .request("session/list", json!({"cwd": ROOT}))
        .await;
    assert_eq!(
        listed["result"]["sessions"][0]["sessionId"], session,
        "{listed}"
    );
    assert_eq!(fixture.run_status(&session), RunStatus::Uncertain);
    connection.abort().await;
}
