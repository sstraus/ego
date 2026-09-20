//! contracts for the workspace posture one ACP session runs at.
//!
//! An ACP session used to be `Untrusted` by construction, which made a host
//! driving ego against a repository the user already trusts a deliberately
//! blind agent with no way to say otherwise. These tests pin the one way it
//! can say otherwise, and every way it cannot.

use std::sync::{Arc, Mutex};

use ego_acp::serve::SessionTrustAdmission;
use ego_core::config::Trust;
use ego_core::run::RunRoot;

mod support;

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/target/regression case");

/// A workspace that says it holds something, and records being asked.
#[derive(Debug, Default)]
struct Lister {
    trustable: Option<String>,
    roots: Mutex<Vec<String>>,
}

impl Lister {
    fn holding(content: &str) -> Arc<Self> {
        Arc::new(Self {
            trustable: Some(content.to_owned()),
            roots: Mutex::new(Vec::new()),
        })
    }

    fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn asked(&self) -> Vec<String> {
        self.roots.lock().expect("asked roots").clone()
    }
}

impl SessionTrustAdmission for Lister {
    fn trustable(&self, root: &RunRoot) -> Option<String> {
        self.roots
            .lock()
            .expect("asked roots")
            .push(root.as_path().display().to_string());
        self.trustable.clone()
    }
}

fn router(
    lister: Option<Arc<dyn SessionTrustAdmission>>,
    executor: Arc<support::Executor>,
) -> agent_client_protocol::AgentProtocolRouter {
    support::agent_with_compaction_confinement_and_trust(
        Arc::new(support::Store::default()),
        executor,
        Arc::new(support::TestClock),
        Arc::new(support::TestIds::default()),
        Arc::new(support::CompactionCommandFixture),
        None,
        None,
        lister,
    )
}

async fn opened(connection: &mut support::Connected, forms: bool) -> String {
    let capabilities = if forms {
        serde_json::json!({ "elicitation": { "form": {} } })
    } else {
        serde_json::json!({})
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.initialize_with_capabilities(capabilities),
    )
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

/// Send one prompt and wait for the trust form it raises, unanswered.
///
/// The elicitation is the agent's own request, so the prompt response cannot
/// arrive before it is answered — which is the point: the posture is settled
/// before preparation, not alongside it.
async fn asked(connection: &mut support::Connected, session: &str) -> (u64, serde_json::Value) {
    let prompt = connection
        .send_request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "hello" }],
            }),
        )
        .await;
    let elicitation = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.agent_request(),
    )
    .await
    .expect("the agent asks before it prepares");
    assert!(
        elicitation["params"]["message"]
            .as_str()
            .expect("the question has a message")
            .contains("Trust the workspace"),
        "{elicitation}"
    );
    (prompt, elicitation)
}

/// Drive one prompt, answering the trust elicitation with `action`.
async fn prompt_answering(
    connection: &mut support::Connected,
    session: &str,
    action: serde_json::Value,
) -> serde_json::Value {
    let (prompt, elicitation) = asked(connection, session).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.answer(&elicitation, action.clone()),
    )
    .await
    .expect("the answer is accepted");
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.response(prompt),
    )
    .await
    .expect("prompt response")
    .0
}

/// Withdraw the turn in flight on this session.
async fn withdraw(connection: &mut support::Connected, session: &str) {
    connection
        .notify(
            "session/cancel",
            serde_json::json!({ "sessionId": session }),
        )
        .await;
}

/// Prove the agent has processed everything sent before this call.
///
/// A plain round-trip: one connection handles its inbound messages in order on
/// one event loop, so an answered request proves every message sent ahead of
/// it was handled. Without it a test would be asserting on a notification that
/// may still be in flight.
async fn fence(connection: &mut support::Connected) {
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.announced(ROOT),
    )
    .await
    .expect("the agent answers while a turn is held");
}

#[tokio::test]
async fn a_host_that_grants_trust_prepares_a_trusted_session() {
    let executor = Arc::new(support::Executor::default());
    let lister = Lister::holding("- AGENTS.md\n");
    let mut connection = support::Connected::new(router(
        Some(Arc::clone(&lister) as Arc<dyn SessionTrustAdmission>),
        Arc::clone(&executor),
    ));
    let session = opened(&mut connection, true).await;
    let answer = prompt_answering(
        &mut connection,
        &session,
        serde_json::json!({ "action": "accept", "content": { "answer": "trusted" } }),
    )
    .await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert_eq!(
        executor.prepared_selections().expect("preparation").trust(),
        Some(Trust::Trusted)
    );
    assert_eq!(lister.asked().len(), 1, "the question is asked once");
    connection.abort().await;
}

#[tokio::test]
async fn a_host_that_keeps_the_session_bare_prepares_an_untrusted_session() {
    let executor = Arc::new(support::Executor::default());
    let mut connection = support::Connected::new(router(
        Some(Lister::holding("- AGENTS.md\n") as Arc<dyn SessionTrustAdmission>),
        Arc::clone(&executor),
    ));
    let session = opened(&mut connection, true).await;
    let answer = prompt_answering(
        &mut connection,
        &session,
        serde_json::json!({ "action": "accept", "content": { "answer": "untrusted" } }),
    )
    .await;
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert_eq!(
        executor.prepared_selections().expect("preparation").trust(),
        Some(Trust::Untrusted)
    );
    connection.abort().await;
}

/// A refusal is a decision; a withdrawal is not. Both run bare.
///
/// They stay two values rather than one so the host that produced them can
/// still tell them apart afterwards — the acceptance criterion this pins.
#[tokio::test]
async fn a_refusal_is_recorded_and_a_withdrawal_is_not_and_neither_is_trusted() {
    for (action, expected) in [
        (
            serde_json::json!({ "action": "decline" }),
            Some(Trust::Untrusted),
        ),
        (serde_json::json!({ "action": "cancel" }), None),
    ] {
        let executor = Arc::new(support::Executor::default());
        let mut connection = support::Connected::new(router(
            Some(Lister::holding("- AGENTS.md\n") as Arc<dyn SessionTrustAdmission>),
            Arc::clone(&executor),
        ));
        let session = opened(&mut connection, true).await;
        let answer = prompt_answering(&mut connection, &session, action.clone()).await;
        assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
        assert_eq!(
            executor.prepared_selections().expect("preparation").trust(),
            expected,
            "{action} must prepare {expected:?}"
        );
        connection.abort().await;
    }
}

/// A host with no form is asked nothing and runs exactly as it did before.
#[tokio::test]
async fn a_host_without_form_elicitation_is_never_asked_and_stays_untrusted() {
    let executor = Arc::new(support::Executor::default());
    let mut connection = support::Connected::new(router(
        Some(Lister::holding("- AGENTS.md\n") as Arc<dyn SessionTrustAdmission>),
        Arc::clone(&executor),
    ));
    let session = opened(&mut connection, false).await;
    let (answer, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "hello" }],
            }),
        ),
    )
    .await
    .expect("a host with no form is not waited on");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert_eq!(
        executor.prepared_selections().expect("preparation").trust(),
        None
    );
    connection.abort().await;
}

/// A workspace holding nothing trust admits is never asked about.
///
/// `trusted` and `untrusted` would open the same session, and a question
/// whose answers do not differ teaches the reader that trust questions do not
/// matter.
#[tokio::test]
async fn an_empty_workspace_raises_no_form_at_all() {
    let executor = Arc::new(support::Executor::default());
    let lister = Lister::empty();
    let mut connection = support::Connected::new(router(
        Some(Arc::clone(&lister) as Arc<dyn SessionTrustAdmission>),
        Arc::clone(&executor),
    ));
    let session = opened(&mut connection, true).await;
    let (answer, notifications) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "hello" }],
            }),
        ),
    )
    .await
    .expect("an empty workspace is not waited on");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert!(
        !notifications
            .iter()
            .any(|message| message.get("method").is_some_and(|method| method
                .as_str()
                .is_some_and(|method| method.contains("elicit")))),
        "no elicitation may be raised: {notifications:?}"
    );
    assert_eq!(lister.asked(), vec![ROOT.to_owned()]);
    assert_eq!(
        executor.prepared_selections().expect("preparation").trust(),
        None
    );
    connection.abort().await;
}

/// A `session/cancel` while the form is open starts no run at all.
///
/// A withdrawal is not a refusal. Refusing answers the question and the turn
/// runs bare; withdrawing says the host no longer wants the turn. Preparation
/// derives the whole workspace posture from the answer, so a withdrawn prompt
/// must never reach it — the criterion is that nothing was prepared, not
/// merely that the response reads `cancelled`.
#[tokio::test]
async fn a_cancel_while_the_form_is_open_prepares_nothing() {
    let executor = Arc::new(support::Executor::default());
    let mut connection = support::Connected::new(router(
        Some(Lister::holding("- AGENTS.md\n") as Arc<dyn SessionTrustAdmission>),
        Arc::clone(&executor),
    ));
    let session = opened(&mut connection, true).await;
    let (prompt, _form) = asked(&mut connection, &session).await;
    withdraw(&mut connection, &session).await;
    let (answer, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.response(prompt),
    )
    .await
    .expect("a withdrawn prompt is answered without its question");
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    assert!(
        executor.prepared_selections().is_none(),
        "a withdrawn prompt must not prepare a run"
    );
    connection.abort().await;
}

/// An answer arriving after the withdrawal starts nothing either.
///
/// The host cannot see that ego stopped waiting, so the answer it already had
/// in hand still arrives. It names a posture for a turn that no longer exists
/// and must open no run — least of all a trusted one.
#[tokio::test]
async fn an_answer_that_arrives_after_a_cancel_starts_nothing() {
    let executor = Arc::new(support::Executor::default());
    let mut connection = support::Connected::new(router(
        Some(Lister::holding("- AGENTS.md\n") as Arc<dyn SessionTrustAdmission>),
        Arc::clone(&executor),
    ));
    let session = opened(&mut connection, true).await;
    let (prompt, form) = asked(&mut connection, &session).await;
    withdraw(&mut connection, &session).await;
    let (answer, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.response(prompt),
    )
    .await
    .expect("a withdrawn prompt is answered");
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    connection
        .answer(
            &form,
            serde_json::json!({ "action": "accept", "content": { "answer": "trusted" } }),
        )
        .await;
    fence(&mut connection).await;
    assert!(
        executor.prepared_selections().is_none(),
        "a late answer must not start the run its question was for"
    );
    connection.abort().await;
}

/// A cancel that lands while the run is being admitted still cancels it.
///
/// This is the window the question closes over: the answer has arrived, the
/// admission is being written, and there is no cancel key yet to name the run
/// with. The admission is not raced — abandoning it half-written would leave a
/// run nobody manages — so the withdrawal is applied to the key the moment it
/// exists, before any turn begins.
#[tokio::test]
async fn a_cancel_that_lands_while_the_run_is_admitted_still_cancels_it() {
    let executor = Arc::new(support::Executor::default());
    executor.block_preparation();
    let mut connection = support::Connected::new(router(
        Some(Lister::holding("- AGENTS.md\n") as Arc<dyn SessionTrustAdmission>),
        Arc::clone(&executor),
    ));
    let session = opened(&mut connection, true).await;
    let (prompt, form) = asked(&mut connection, &session).await;
    connection
        .answer(
            &form,
            serde_json::json!({ "action": "accept", "content": { "answer": "trusted" } }),
        )
        .await;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        executor.preparation_started(),
    )
    .await
    .expect("the answered question reaches preparation");
    withdraw(&mut connection, &session).await;
    fence(&mut connection).await;
    executor.release_preparation();
    let (answer, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.response(prompt),
    )
    .await
    .expect("prompt response");
    assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
    // The answer did land: this window is after the question, not before it.
    assert_eq!(
        executor.prepared_selections().expect("preparation").trust(),
        Some(Trust::Trusted)
    );
    connection.abort().await;
}

/// The posture is asked once per session, not once per prompt.
///
/// Every prompt after the first continues the run the first one started, and
/// that run's config layers, instruction file, project MCP servers, skills and
/// hooks were all derived from the first answer. A second question could only
/// produce an answer nothing would act on.
#[tokio::test]
async fn a_trusted_session_is_not_asked_again_on_the_next_prompt() {
    let executor = Arc::new(support::Executor::default());
    let lister = Lister::holding("- AGENTS.md\n");
    let mut connection = support::Connected::new(router(
        Some(Arc::clone(&lister) as Arc<dyn SessionTrustAdmission>),
        Arc::clone(&executor),
    ));
    let session = opened(&mut connection, true).await;
    let first = prompt_answering(
        &mut connection,
        &session,
        serde_json::json!({ "action": "accept", "content": { "answer": "trusted" } }),
    )
    .await;
    assert_eq!(first["result"]["stopReason"], "end_turn", "{first}");
    let (second, messages) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "again" }],
            }),
        ),
    )
    .await
    .expect("a continued turn is not waited on");
    assert_eq!(second["result"]["stopReason"], "end_turn", "{second}");
    assert!(
        !messages
            .iter()
            .any(|message| message.get("method").is_some_and(|method| method
                .as_str()
                .is_some_and(|method| method.contains("elicit")))),
        "the second prompt may raise no question: {messages:?}"
    );
    assert_eq!(lister.asked().len(), 1, "the workspace is listed once");
    connection.abort().await;
}

/// No lister at all is the shape every non-ego host of this router has.
#[tokio::test]
async fn a_router_without_a_trust_port_prepares_exactly_as_before() {
    let executor = Arc::new(support::Executor::default());
    let mut connection = support::Connected::new(router(None, Arc::clone(&executor)));
    let session = opened(&mut connection, true).await;
    let (answer, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        connection.request(
            "session/prompt",
            serde_json::json!({
                "sessionId": session,
                "prompt": [{ "type": "text", "text": "hello" }],
            }),
        ),
    )
    .await
    .expect("prompt response");
    assert_eq!(answer["result"]["stopReason"], "end_turn", "{answer}");
    assert_eq!(
        executor.prepared_selections().expect("preparation").trust(),
        None
    );
    connection.abort().await;
}
