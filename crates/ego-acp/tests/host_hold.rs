//! RED contract for regression case's `_ego/pause` and `_ego/resume` host controls.
//!
//! Every request crosses the pinned SDK's real JSON-RPC router, because the
//! claim under test is a protocol claim: that a host which negotiated the
//! extension can stop a session at a boundary and let it go again, and that a
//! host which did not is told the methods do not exist.
//!
//! The durability claims are proved by asking a *second* router — one that
//! shares only the store — to release a hold the first one recorded. A pause
//! kept in a connection's memory would pass every single-connection assertion
//! here and fail that one.

use ego_core::run::RunStatus;

mod support;

const ROOT: &str = "/tmp/regression case-workspace";
const REQUEST: &str = "00000000-0000-0000-0000-000000000091";
const SECOND_REQUEST: &str = "00000000-0000-0000-0000-000000000092";

fn hold_params(session: &str, request: &str) -> serde_json::Value {
    serde_json::json!({"v": 1, "sessionId": session, "requestId": request})
}

/// The `state` a hold request answered with, or a panic naming the response.
fn state(response: &serde_json::Value) -> &str {
    response["result"]["state"]
        .as_str()
        .unwrap_or_else(|| panic!("a hold request answers with a state: {response}"))
}

#[tokio::test]
async fn initialize_advertises_the_versioned_hold_extension() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    let response = connection.initialize().await;

    let hold = &response["result"]["agentCapabilities"]["_meta"]["ego"]["hold"];
    assert_eq!(hold["version"], 1, "hold advertisement: {response}");
    assert_eq!(hold["pause"], "_ego/pause");
    assert_eq!(hold["resume"], "_ego/resume");

    connection.abort().await;
}

/// The negotiation half of the ADR: an agent built without the lifecycle port
/// cannot stop a session at a durable boundary, so it says so twice — by not
/// advertising the extension, and by answering `method_not_found` to anyone
/// who calls it anyway.
#[tokio::test]
async fn an_agent_without_a_lifecycle_neither_advertises_nor_serves_hold() {
    let mut connection = support::Connected::new(support::agent());
    let response = connection.initialize().await;
    assert!(
        response["result"]["agentCapabilities"]["_meta"]["ego"]
            .get("hold")
            .is_none(),
        "hold was advertised without a lifecycle to serve it: {response}"
    );

    for method in ["_ego/pause", "_ego/resume"] {
        let (response, _) = connection
            .request(method, hold_params(REQUEST, REQUEST))
            .await;
        assert_eq!(
            response["error"]["code"], -32601,
            "{method} without a lifecycle: {response}"
        );
    }

    connection.abort().await;
}

#[tokio::test]
async fn pausing_an_idle_session_records_the_boundary_and_answers_paused() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();

    let (response, _) = connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;

    assert_eq!(state(&response), "paused", "idle pause: {response}");
    assert_eq!(response["result"]["sessionId"], session);
    assert_eq!(response["result"]["requestId"], REQUEST);
    assert_eq!(response["result"]["v"], 1);

    connection.abort().await;
}

/// Idempotence through the log, not through a receipt store. A second pause —
/// under a *different* request id, which is the harder case — finds the hold
/// already in force and neither displaces it nor establishes a second one.
/// The single `_ego/resume` at the end is the proof: if the repeat had stacked
/// a second hold, one release would not have been enough.
#[tokio::test]
async fn a_repeated_pause_neither_displaces_the_hold_nor_establishes_a_second() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();

    for request in [REQUEST, SECOND_REQUEST] {
        let (response, _) = connection
            .request("_ego/pause", hold_params(&session, request))
            .await;
        assert_eq!(state(&response), "paused", "pause {request}: {response}");
    }
    assert_eq!(
        fixture.recorded_holds(&session),
        1,
        "the repeat established a second hold instead of finding the first"
    );

    let (released, _) = connection
        .request("_ego/resume", hold_params(&session, REQUEST))
        .await;
    assert_eq!(state(&released), "running", "one release: {released}");

    connection.abort().await;
}

/// `_ego/resume` accepts only a durably paused turn. A session that was never
/// held has nothing to release, and saying "running" back would let a host
/// believe it had undone a pause that never happened.
#[tokio::test]
async fn resume_refuses_a_session_that_is_not_paused() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();

    for attempt in ["never held", "already released"] {
        let (response, _) = connection
            .request("_ego/resume", hold_params(&session, REQUEST))
            .await;
        let message = response["error"]["data"]
            .as_str()
            .unwrap_or_else(|| panic!("{attempt} resume is refused with a reason: {response}"));
        assert!(
            message.contains("not paused") && message.contains("running"),
            "{attempt}: {message}"
        );
        // The second pass runs after a pause and a release, so the refusal
        // has to survive a session that *was* held once.
        if attempt == "never held" {
            connection
                .request("_ego/pause", hold_params(&session, REQUEST))
                .await;
            connection
                .request("_ego/resume", hold_params(&session, REQUEST))
                .await;
        }
    }

    connection.abort().await;
}

/// A running turn cannot be stopped by writing a boundary on its behalf: it
/// holds the lease, and it is the only writer that can say truthfully where it
/// stopped. The host is told `pending` and not `paused`, which is the whole
/// reason the two words are separate on the wire.
#[tokio::test]
async fn pausing_a_running_turn_answers_pending_and_not_paused() {
    let fixture = support::DurableAcpFixture::new();
    let session = fixture.seed_running_session(ROOT);
    assert_eq!(fixture.run_status(&session), RunStatus::Running);

    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let (response, _) = connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;

    assert_eq!(state(&response), "pending", "running pause: {response}");

    connection.abort().await;
}

/// A pending hold is not a paused one, and releasing it would let the turn
/// still walking to its boundary carry on as though nobody had asked.
///
/// The refusal names the active work and deliberately not the hold. A pending
/// hold has no durable record — that is what makes it pending — so the session
/// log cannot distinguish "asked and not there yet" from "never asked", and
/// answering `not paused; it is running` would contradict the `pending` this
/// same host was handed a moment earlier. "Active work" contradicts neither.
#[tokio::test]
async fn resume_refuses_a_hold_that_has_not_reached_its_boundary() {
    let fixture = support::DurableAcpFixture::new();
    let session = fixture.seed_running_session(ROOT);
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let (pending, _) = connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;
    assert_eq!(state(&pending), "pending");

    let (response, _) = connection
        .request("_ego/resume", hold_params(&session, REQUEST))
        .await;

    let message = response["error"]["data"]
        .as_str()
        .unwrap_or_else(|| panic!("a pending resume is refused: {response}"));
    assert!(message.contains("active work"), "pending resume: {message}");
    assert_eq!(
        fixture.run_status(&session),
        RunStatus::Running,
        "the refused resume must not have moved the run"
    );

    connection.abort().await;
}

/// The durability claim. The router that recorded the hold is gone by the time
/// the second one is asked to release it, and the two share only the store.
#[tokio::test]
async fn a_hold_outlives_the_connection_that_asked_for_it() {
    let fixture = support::DurableAcpFixture::new();
    let mut first = support::Connected::new(fixture.router());
    first.initialize().await;
    let session = first.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();
    let (paused, _) = first
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;
    assert_eq!(state(&paused), "paused");
    first.abort().await;

    let mut second = support::Connected::new(fixture.router());
    second.initialize().await;
    let (released, _) = second
        .request("_ego/resume", hold_params(&session, REQUEST))
        .await;

    assert_eq!(
        state(&released),
        "running",
        "a reconnecting host could not release the hold: {released}"
    );

    second.abort().await;
}

#[tokio::test]
async fn a_malformed_hold_request_is_refused_before_the_lifecycle_is_asked() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();

    for (name, params) in [
        (
            "unsupported version",
            serde_json::json!({"v": 2, "sessionId": session, "requestId": REQUEST}),
        ),
        (
            "unknown field",
            serde_json::json!({
                "v": 1, "sessionId": session, "requestId": REQUEST, "force": true
            }),
        ),
        (
            "nil sessionId",
            hold_params("00000000-0000-0000-0000-000000000000", REQUEST),
        ),
        (
            "nil requestId",
            hold_params(&session, "00000000-0000-0000-0000-000000000000"),
        ),
        ("unparseable sessionId", hold_params("not-a-uuid", REQUEST)),
    ] {
        for method in ["_ego/pause", "_ego/resume"] {
            let (response, _) = connection.request(method, params.clone()).await;
            assert_eq!(
                response["error"]["code"], -32602,
                "{method} with {name}: {response}"
            );
        }
    }

    // The session is still untouched: every refusal above happened before any
    // hold could be recorded.
    let (response, _) = connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;
    assert_eq!(state(&response), "paused", "after refusals: {response}");

    connection.abort().await;
}
