//! RED router contract for the ACP session config options (regression case).
//!
//! Every request below crosses the pinned SDK's real v1 router, because the
//! router is the only thing that can say whether a method is reachable. The
//! projection tests in `controls.rs` prove the shape; these prove the
//! negotiation: a connection that owns no operator seat must offer no picker
//! and answer `method_not_found`, and a connection that owns one must publish
//! it on the session it opened.

use std::sync::{Arc, Mutex};

use ego_core::config::{Mode, ModelRef};
use ego_core::control::{
    ControlError, ControlSelect, SessionControlChange, SessionControlState, SessionControls,
};
use ego_core::event::id::SessionId;
use ego_core::generator::types::ThinkingLevel;
use ego_core::permit::Confinement;

mod support;

const ROOT: &str = "/tmp/regression case";

/// A seat that answers for one session and remembers what was staged.
#[derive(Debug, Default)]
struct Seat {
    staged: Mutex<Vec<(SessionId, SessionControlChange)>>,
}

impl Seat {
    fn state(&self) -> SessionControlState {
        let staged = self.staged.lock().expect("seat lock");
        let mode = staged.iter().rev().find_map(|(_, change)| match change {
            SessionControlChange::Mode(mode) => Some(*mode),
            _ => None,
        });
        SessionControlState {
            model: ControlSelect {
                current: ModelRef("ollama/gemma4:e4b-mlx".to_owned()),
                chosen: None,
                available: vec![ModelRef("ollama/gemma4:e4b-mlx".to_owned())],
            },
            effort: ControlSelect {
                current: ThinkingLevel::Off,
                chosen: None,
                available: vec![ThinkingLevel::Off],
            },
            mode: ControlSelect {
                current: mode.unwrap_or(Mode::Default),
                chosen: mode,
                available: vec![Mode::Plan, Mode::Default],
            },
            sandbox: ControlSelect {
                current: Confinement::Workspace,
                chosen: None,
                available: vec![Confinement::Ro, Confinement::Workspace],
            },
        }
    }
}

impl SessionControls for Seat {
    fn state(&self, _session: SessionId) -> Result<SessionControlState, ControlError> {
        Ok(Seat::state(self))
    }

    fn stage(
        &self,
        session: SessionId,
        change: SessionControlChange,
    ) -> Result<SessionControlState, ControlError> {
        self.staged
            .lock()
            .expect("seat lock")
            .push((session, change));
        Ok(Seat::state(self))
    }
}

fn router(seat: Option<Arc<dyn SessionControls>>) -> agent_client_protocol::AgentProtocolRouter {
    support::agent_with_compaction(
        Arc::new(support::Store::default()),
        Arc::new(support::Executor::default()),
        Arc::new(support::TestClock),
        Arc::new(support::TestIds::default()),
        Arc::new(support::CompactionCommandFixture),
        seat,
    )
}

fn option<'a>(response: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    response["result"]["configOptions"]
        .as_array()
        .unwrap_or_else(|| panic!("no configOptions in {response}"))
        .iter()
        .find(|option| option["id"] == id)
        .unwrap_or_else(|| panic!("no `{id}` option in {response}"))
}

/// A session opened by a connection that owns an operator seat publishes the
/// picker on `session/new`, because ACP v1 has no capability flag for config
/// options: returning them *is* the advertisement.
#[tokio::test]
async fn a_session_publishes_its_controls_when_the_connection_owns_a_seat() {
    let mut connection = support::Connected::new(router(Some(Arc::new(Seat::default()))));
    connection.initialize().await;
    let session = connection.new_session(ROOT).await;
    assert_eq!(option(&session, "model")["category"], "model");
    assert_eq!(option(&session, "effort")["category"], "thought_level");
    assert_eq!(option(&session, "mode")["currentValue"], "default");
    connection.abort().await;
}

/// No seat is not an empty picker. A host must be able to tell "ego has no
/// controls here" from "ego has controls and none are set", which is the same
/// rule `docs/06-acp.md` §3 applies to absent usage and cost.
#[tokio::test]
async fn a_connection_without_a_seat_publishes_no_picker_at_all() {
    let mut connection = support::Connected::new(router(None));
    connection.initialize().await;
    let session = connection.new_session(ROOT).await;
    assert!(
        session["result"].get("configOptions").is_none(),
        "an absent seat must not publish an empty picker: {session}"
    );
    connection.abort().await;
}

/// An unadvertised control is unavailable, not partially emulated.
#[tokio::test]
async fn setting_a_config_option_without_a_seat_is_method_not_found() {
    let mut connection = support::Connected::new(router(None));
    connection.initialize().await;
    let session = connection.new_session(ROOT).await;
    let id = session["result"]["sessionId"]
        .as_str()
        .expect("a session id");
    let (answer, _) = connection
        .request(
            "session/set_config_option",
            serde_json::json!({"sessionId": id, "configId": "mode", "value": "plan"}),
        )
        .await;
    assert_eq!(answer["error"]["code"], -32601, "{answer}");
    connection.abort().await;
}

/// The response is the whole replacement set, so a host that changes one
/// control learns what that did to the others in the same round trip.
#[tokio::test]
async fn a_staged_change_answers_with_the_whole_replacement_set() {
    let seat = Arc::new(Seat::default());
    let mut connection =
        support::Connected::new(router(Some(Arc::clone(&seat) as Arc<dyn SessionControls>)));
    connection.initialize().await;
    let session = connection.new_session(ROOT).await;
    let id = session["result"]["sessionId"]
        .as_str()
        .expect("a session id");
    let (answer, _) = connection
        .request(
            "session/set_config_option",
            serde_json::json!({"sessionId": id, "configId": "mode", "value": "plan"}),
        )
        .await;
    assert_eq!(option(&answer, "mode")["currentValue"], "plan", "{answer}");
    assert_eq!(
        option(&answer, "model")["currentValue"],
        "ollama/gemma4:e4b-mlx"
    );
    let staged = seat.staged.lock().expect("seat lock").clone();
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].0.to_string(), id);
    assert_eq!(staged[0].1, SessionControlChange::Mode(Mode::Plan));
    connection.abort().await;
}

/// A value the session never offered never reaches the seat. Refusing at the
/// projection is what keeps "the seat was asked" and "the seat agreed" from
/// being the same event.
#[tokio::test]
async fn an_unoffered_value_is_refused_before_the_seat_is_asked() {
    let seat = Arc::new(Seat::default());
    let mut connection =
        support::Connected::new(router(Some(Arc::clone(&seat) as Arc<dyn SessionControls>)));
    connection.initialize().await;
    let session = connection.new_session(ROOT).await;
    let id = session["result"]["sessionId"]
        .as_str()
        .expect("a session id");
    let (answer, _) = connection
        .request(
            "session/set_config_option",
            serde_json::json!({"sessionId": id, "configId": "mode", "value": "yolo"}),
        )
        .await;
    assert_eq!(answer["error"]["code"], -32602, "{answer}");
    assert!(
        seat.staged.lock().expect("seat lock").is_empty(),
        "a refused value must never reach the seat"
    );
    connection.abort().await;
}
