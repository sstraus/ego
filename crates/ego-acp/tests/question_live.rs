//! RED contract for ACP live elicitation integration.

use futures::executor::block_on;
use std::sync::Arc;

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

#[test]
fn outgoing_live_request_uses_the_durable_question_item_as_its_scope() {
    let id = ItemId::new();
    let outgoing = AcpQuestionAdapter::form_request(id, &request()).unwrap();
    let rendered = format!("{outgoing:?}");
    assert!(rendered.contains(&id.to_string()));
}

#[test]
fn capability_absence_becomes_an_unavailable_question_outcome() {
    let adapter = AcpQuestionAdapter::new(CapabilityCapture::default());
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
