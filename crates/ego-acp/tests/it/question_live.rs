//! contract for ACP live elicitation integration.

use futures::executor::block_on;
use std::sync::Arc;

use agent_client_protocol_schema::v1;
use ego_acp::question::{AcpQuestionAdapter, CapabilityCapture};
use ego_core::event::id::ItemId;
use ego_core::question::{Elicitor, QuestionMode, QuestionOption, QuestionRequest};

fn request() -> QuestionRequest {
    QuestionRequest::new(
        "Choose",
        QuestionMode::Single,
        vec![QuestionOption::new("yes", "Yes")],
        false,
    )
}

/// A question names the session that asked it.
///
/// This replaces an assertion that the outgoing request carried the durable
/// question item as its scope. That was request scope, which ACP defines for
/// what happens *outside* a session — before one is started — and it left a
/// host with no way to tell which of the sessions it has attached is asking.
/// The durable item is not lost by this: it never leaves ego, and it does not
/// need to, because the answer arrives as the response to this very request.
#[test]
fn outgoing_live_request_is_scoped_to_the_session_that_asked() {
    let session = v1::SessionId::new("01932d5e-0000-7000-8000-0000000000aa");
    let outgoing = AcpQuestionAdapter::form_request(&session, ItemId::new(), &request()).unwrap();

    let v1::ElicitationScope::Session(scope) = outgoing.scope() else {
        panic!("a question asked inside a session is session-scoped: {outgoing:?}");
    };
    assert_eq!(scope.session_id, session);
    // Not a tool call, and the optional field says so rather than being filled
    // with the nearest id to hand.
    assert!(scope.tool_call_id.is_none());
}

/// Two sessions asking at once stay apart.
///
/// One connection can have several sessions attached, and each question has to
/// name its own. This is the case an unattributed scope cannot serve at all.
#[test]
fn two_sessions_asking_at_once_are_told_apart_by_their_scope() {
    let first = v1::SessionId::new("01932d5e-0000-7000-8000-0000000000aa");
    let second = v1::SessionId::new("01932d5e-0000-7000-8000-0000000000bb");

    let scope_of = |session: &v1::SessionId| {
        let outgoing =
            AcpQuestionAdapter::form_request(session, ItemId::new(), &request()).unwrap();
        match outgoing.scope() {
            v1::ElicitationScope::Session(scope) => scope.session_id.clone(),
            other => panic!("session scope expected: {other:?}"),
        }
    };

    assert_eq!(scope_of(&first), first);
    assert_eq!(scope_of(&second), second);
}

#[test]
fn capability_absence_becomes_an_unavailable_question_outcome() {
    let adapter = AcpQuestionAdapter::new(
        v1::SessionId::new("01932d5e-0000-7000-8000-0000000000aa"),
        CapabilityCapture::default(),
    );
    let id = ItemId::new();
    let outcome = block_on(adapter.elicitor().elicit(id, request()))
        .expect("missing capability is represented as unavailable");
    assert!(matches!(
        outcome,
        ego_core::question::QuestionOutcome::Unavailable
    ));
}

// ACP must be the host implementation of core's one async/id-aware port;
// serve must pass the same adapter into TurnLoop rather than settling in a
// projection callback or emitting duplicate question updates.
#[allow(dead_code)]
fn acp_exposes_the_core_elicitor(adapter: &AcpQuestionAdapter) -> Arc<dyn Elicitor> {
    adapter.elicitor()
}

#[test]
fn url_member_without_host_capability_stays_pending_instead_of_becoming_a_secret_prompt() {
    // Catches: the form-only ACP capability is used to solicit a password or copied URL code.
    use ego_core::question::{
        ElicitationKind, ElicitationMember, ElicitationRound, QuestionOutcome,
    };
    let request = QuestionRequest::new(
        "MCP server alpha requires input",
        QuestionMode::Round(ElicitationRound {
            expires_at_ms: None,
            requests: std::collections::BTreeMap::from([(
                "verify".to_owned(),
                ElicitationMember {
                    prompt: "Complete on page".to_owned(),
                    kind: ElicitationKind::Url {
                        url: "https://example.test/verify".to_owned(),
                        elicitation_id: "verify-42".to_owned(),
                    },
                },
            )]),
            completed: Default::default(),
        }),
        vec![],
        false,
    );
    let adapter = AcpQuestionAdapter::new(
        v1::SessionId::new("session-round"),
        CapabilityCapture::form(),
    );
    assert_eq!(
        block_on(adapter.elicit(ItemId::new(), request)).unwrap(),
        QuestionOutcome::Unavailable
    );
}
