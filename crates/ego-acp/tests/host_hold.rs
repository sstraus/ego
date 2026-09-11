//! contract for regression case's `_ego/pause` and `_ego/resume` host controls.
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
const THIRD_REQUEST: &str = "00000000-0000-0000-0000-000000000093";
const FOURTH_REQUEST: &str = "00000000-0000-0000-0000-000000000094";

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

/// The other half of idempotence, and the one a request-id receipt cannot
/// give: a second pause under a *different* id finds the hold already in force
/// and neither displaces it nor establishes a second one. Two hosts sharing a
/// session are the real case. The single `_ego/resume` at the end is the
/// proof — if the repeat had stacked a second hold, one release would not have
/// been enough.
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

/// The lost-response retry. The release committed and the answer never
/// arrived, so the host asks again with the id it already used — the one case
/// an idempotency key exists for. Answering "not paused" would tell that host
/// its request had failed when it had in fact succeeded, and the difference is
/// visible: it would go on to re-issue the pause it thought it still held.
///
/// Idempotence by *current state* cannot do this. The state has moved on; only
/// the record of what request R was answered with can replay R's answer.
#[tokio::test]
async fn a_resume_that_already_released_replays_its_answer_instead_of_refusing() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();
    connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;

    let mut answers = Vec::new();
    for _ in 0..2 {
        let (response, _) = connection
            .request("_ego/resume", hold_params(&session, SECOND_REQUEST))
            .await;
        answers.push(response);
    }

    assert_eq!(
        state(&answers[0]),
        "running",
        "first release: {:?}",
        answers[0]
    );
    assert_eq!(
        state(&answers[1]),
        "running",
        "the retry was refused instead of replayed: {:?}",
        answers[1]
    );

    connection.abort().await;
}

/// The delayed retry. A pause was answered, a resume released it, and only
/// then does the transport re-deliver the original pause. Replaying its
/// recorded answer is the whole point: acting on it would re-pause a session
/// the host had deliberately let go, and nothing in the second delivery says
/// it is not the first.
#[tokio::test]
async fn a_pause_redelivered_after_its_release_replays_rather_than_holding_again() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();
    connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;
    connection
        .request("_ego/resume", hold_params(&session, SECOND_REQUEST))
        .await;

    let (redelivered, _) = connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;

    assert_eq!(
        state(&redelivered),
        "paused",
        "a redelivered request answers what it was answered: {redelivered}"
    );
    assert_eq!(
        fixture.recorded_holds(&session),
        1,
        "the redelivered pause held the session a second time"
    );

    connection.abort().await;
}

/// A second host told `paused` is owed the same replay as the host that asked
/// first.
///
/// `paused` promises the durable boundary exists, and a host cannot see that
/// the boundary carries someone else's request id. If only the establishing
/// request is remembered, the second host's word is worth less than the first
/// one's: its retry after a release finds no receipt and holds a session
/// nobody is asking to hold — the same defect as a redelivered pause, reached
/// by the one path that writes nothing.
#[tokio::test]
async fn a_hold_a_second_host_was_told_of_replays_like_the_one_that_asked() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();
    connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;

    // The second host asks for a hold that is already in force and is told so.
    let (observed, _) = connection
        .request("_ego/pause", hold_params(&session, SECOND_REQUEST))
        .await;
    assert_eq!(state(&observed), "paused", "the hold in force: {observed}");

    connection
        .request("_ego/resume", hold_params(&session, THIRD_REQUEST))
        .await;

    // Its reply was lost in transit, so it asks again — after the release.
    let (redelivered, _) = connection
        .request("_ego/pause", hold_params(&session, SECOND_REQUEST))
        .await;
    assert_eq!(
        state(&redelivered),
        "paused",
        "the second host's request replays its own answer: {redelivered}"
    );
    assert_eq!(
        fixture.recorded_holds(&session),
        1,
        "the second host's redelivered pause held the session again"
    );

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

    // Every attempt spends a fresh request id. Reusing one would stop
    // testing refusal and start testing replay, which is a different rule and
    // has its own test: a request that already released a hold is answered
    // again rather than refused.
    for (attempt, request) in [
        ("never held", REQUEST),
        ("already released", FOURTH_REQUEST),
    ] {
        let (response, _) = connection
            .request("_ego/resume", hold_params(&session, request))
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
                .request("_ego/pause", hold_params(&session, SECOND_REQUEST))
                .await;
            connection
                .request("_ego/resume", hold_params(&session, THIRD_REQUEST))
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

/// A refusal says whose fault it was, because that is what a host acts on.
///
/// `invalid_params` is a permanent verdict on what the caller sent: a host
/// that reads it correctly stops and shows the operator its request was
/// malformed. Neither of these is that. A session id that is well-formed but
/// names nothing is a missing resource, and a lease another writer holds for
/// the length of one append is a transient the same request can win a moment
/// later — the method is idempotent precisely so it can be retried. Telling
/// either one "invalid params" makes a host give up on a request that was
/// never wrong.
#[tokio::test]
async fn a_refusal_names_a_missing_session_and_a_busy_one_as_what_they_are() {
    let fixture = support::DurableAcpFixture::new();
    let mut connection = support::Connected::new(fixture.router());
    connection.initialize().await;
    let session = connection.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();

    // Well-formed, parses, is not nil — and names no session.
    let (missing, _) = connection
        .request("_ego/pause", hold_params(SECOND_REQUEST, REQUEST))
        .await;
    assert_eq!(
        missing["error"]["code"], -32002,
        "a session that does not exist is a missing resource: {missing}"
    );

    // Another writer owns the session for the length of its append.
    let held = fixture.hold_lease(&session);
    let (busy, _) = connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;
    assert_eq!(
        busy["error"]["code"], -32010,
        "a lease held for one append is transient, not malformed: {busy}"
    );
    drop(held);

    // And the retry the code invited succeeds, on the same request id.
    let (retried, _) = connection
        .request("_ego/pause", hold_params(&session, REQUEST))
        .await;
    assert_eq!(state(&retried), "paused", "the invited retry: {retried}");

    connection.abort().await;
}

/// Two hosts on one session, holding it at the same moment.
///
/// The hold is a fact about the session and not about the connection that
/// asked, so the log records one hold and not two. A per-connection hold would
/// have let each host believe it owned a separate one, and the first release
/// would have freed a session the other still thought it was holding.
///
/// What is *not* asserted is that both hosts are answered `paused`. The loser
/// of the writer-lease race is refused, and whether a given run produces a
/// loser at all depends on the scheduler; asserting both answers would be
/// asserting a linearization the lease does not provide and would make this
/// test flaky rather than strict. One hold between them is the contract, and
/// it holds on every interleaving.
// A real thread race and not cooperative interleaving: on the single-threaded
// runtime the two round trips would take turns at their first await and prove
// only that the second request sees the first one's result. The lease is what
// has to hold here, and only two threads can lean on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_connections_holding_at_once_establish_one_hold_between_them() {
    let fixture = support::DurableAcpFixture::new();
    let mut opener = support::Connected::new(fixture.router());
    opener.initialize().await;
    let session = opener.new_session(ROOT).await["result"]["sessionId"]
        .as_str()
        .expect("session/new returns a session id")
        .to_owned();

    let mut first = support::Connected::new(fixture.router());
    let mut second = support::Connected::new(fixture.router());
    first.initialize().await;
    second.initialize().await;
    let (left, right) = futures::join!(
        first.request("_ego/pause", hold_params(&session, REQUEST)),
        second.request("_ego/pause", hold_params(&session, SECOND_REQUEST)),
    );

    let answered: Vec<_> = [&left.0, &right.0]
        .into_iter()
        .filter(|response| response["result"]["state"] == "paused")
        .collect();
    assert!(
        !answered.is_empty(),
        "neither host was told the session is paused: {:?} / {:?}",
        left.0,
        right.0
    );
    assert_eq!(
        fixture.recorded_holds(&session),
        1,
        "two hosts established two holds on one session"
    );

    // And one release frees it, because there was only ever one hold. The
    // host that releases need not be the host that asked.
    let (released, _) = second
        .request("_ego/resume", hold_params(&session, THIRD_REQUEST))
        .await;
    assert_eq!(state(&released), "running", "one release: {released}");

    first.abort().await;
    second.abort().await;
    opener.abort().await;
}
