//! RED contracts for ACP confinement selection at the real v1 router.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use ego_core::config::{Mode, ModelRef};
use ego_core::control::{
    ControlError, ControlSelect, SessionControlChange, SessionControlState, SessionControls,
};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;

mod support;

const ROOT: &str = "/tmp/regression case";

#[derive(Debug, Default)]
struct Seat {
    unreadable: AtomicBool,
    changes: Mutex<Vec<(SessionId, SessionControlChange)>>,
}

impl SessionControls for Seat {
    fn state(&self, _session: SessionId) -> Result<SessionControlState, ControlError> {
        if self.unreadable.load(Ordering::SeqCst) {
            return Err(ControlError::Refused {
                control: "sandbox",
                why: "state is unreadable".to_owned(),
            });
        }
        Ok(SessionControlState {
            model: ControlSelect {
                current: Some(ModelRef("model/test".into())),
                chosen: None,
                available: vec![],
            },
            effort: ControlSelect {
                current: Some(ThinkingLevel::Off),
                chosen: None,
                available: vec![],
            },
            mode: ControlSelect {
                current: Some(Mode::Default),
                chosen: None,
                available: vec![],
            },
            sandbox: ControlSelect {
                current: Some(Confinement::Ro),
                chosen: None,
                available: vec![Confinement::Ro, Confinement::Workspace],
            },
        })
    }

    fn stage(
        &self,
        session: SessionId,
        change: SessionControlChange,
    ) -> Result<SessionControlState, ControlError> {
        self.changes
            .lock()
            .expect("changes")
            .push((session, change));
        self.state(session)
    }
}

fn router(
    seat: Arc<Seat>,
    executor: Arc<support::Executor>,
) -> agent_client_protocol::AgentProtocolRouter {
    support::agent_with_compaction(
        Arc::new(support::Store::default()),
        executor,
        Arc::new(support::TestClock),
        Arc::new(support::TestIds::default()),
        Arc::new(support::CompactionCommandFixture),
        Some(seat),
    )
}

async fn opened(connection: &mut support::Connected) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(1), connection.initialize())
        .await
        .expect("initialize response");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.new_session(ROOT),
    )
    .await
    .expect("session/new response")["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_owned()
}

#[tokio::test]
async fn current_sandbox_is_carried_into_run_preparation_when_unchosen() {
    let executor = Arc::new(support::Executor::default());
    let mut connection =
        support::Connected::new(router(Arc::new(Seat::default()), Arc::clone(&executor)));
    let session = opened(&mut connection).await;
    let (answer, _) = connection.request("session/prompt", serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "hello"}]})).await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert_eq!(
        executor
            .prepared_selections()
            .expect("preparation")
            .sandbox(),
        Some(Confinement::Ro)
    );
    connection.abort().await;
}

#[tokio::test]
async fn unreadable_control_state_refuses_before_executor_prepare() {
    let executor = Arc::new(support::Executor::default());
    let seat = Arc::new(Seat::default());
    let mut connection = support::Connected::new(router(Arc::clone(&seat), Arc::clone(&executor)));
    let session = opened(&mut connection).await;
    seat.unreadable.store(true, Ordering::SeqCst);
    let (answer, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.request("session/prompt", serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "hello"}]})),
    ).await.expect("prompt refusal");
    assert!(answer.get("error").is_some(), "{answer}");
    assert!(
        executor.prepared_selections().is_none(),
        "executor.prepare must not run"
    );
    connection.abort().await;
}

#[tokio::test]
async fn unknown_sandbox_value_is_refused_by_the_router() {
    let executor = Arc::new(support::Executor::default());
    let mut connection = support::Connected::new(router(Arc::new(Seat::default()), executor));
    let session = opened(&mut connection).await;
    let (answer, _) = connection
        .request(
            "session/set_config_option",
            serde_json::json!({"sessionId": session, "configId": "sandbox", "value": "unknown"}),
        )
        .await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("unknown")
    );
    assert!(
        answer["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("sandbox")
    );
    connection.abort().await;
}

#[tokio::test]
async fn sandbox_stage_is_refused_while_the_first_prompt_is_preparing() {
    let seat = Arc::new(Seat::default());
    let executor = Arc::new(support::Executor::default());
    executor.block_preparation();
    let mut connection = support::Connected::new(router(Arc::clone(&seat), Arc::clone(&executor)));
    let session = opened(&mut connection).await;
    let prompt_id = connection.send_request("session/prompt", serde_json::json!({"sessionId": session, "prompt": [{"type": "text", "text": "hello"}]})).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        executor.preparation_started(),
    )
    .await
    .expect("preparation gate");
    let stage_id = connection
        .send_request(
            "session/set_config_option",
            serde_json::json!({"sessionId": session, "configId": "sandbox", "value": "workspace"}),
        )
        .await;
    executor.release_preparation();
    let prompt = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.response(prompt_id),
    )
    .await;
    let (prompt, notifications) = match prompt {
        Ok(value) => value,
        Err(error) => {
            executor.release_preparation();
            panic!("prompt response did not arrive after preparation release: {error}");
        }
    };
    assert_eq!(prompt["result"]["stopReason"], "end_turn", "{prompt}");
    let stage = if let Some(stage) = notifications
        .into_iter()
        .find(|message| message["id"].as_u64() == Some(stage_id))
    {
        stage
    } else {
        // The router may answer the prompt before the queued control request
        // writes its response. A read-only fence drains that later frame
        // without depending on a timing-based sleep or a quiet stream.
        connection
            .announced(ROOT)
            .await
            .into_iter()
            .find(|message| message["id"].as_u64() == Some(stage_id))
            .expect("sandbox stage response was collected before the fence response")
    };
    assert!(
        stage.get("error").is_some(),
        "stage applied during preparation: {stage}"
    );
    assert!(seat.changes.lock().expect("changes").is_empty());
    connection.abort().await;
}
