//! RED runtime contract for one-turn question suspension.
//!
//! This module deliberately exercises the core-owned async port and the
//! TurnLoop attachment point.  Host adapters are not allowed to become a
//! second parser or to manufacture a question id: the id passed to Elicitor
//! is the persisted QuestionRequested envelope ItemId.

use std::sync::{Arc, Mutex};

use ego_core::event::id::ItemId;
use ego_core::question::{
    Elicitor, ElicitorError, QuestionAnswer, QuestionMode, QuestionOption, QuestionOutcome,
    QuestionRequest,
};
use ego_core::turn::TurnLoop;
use futures::future::BoxFuture;

#[derive(Default)]
struct RecordingElicitor {
    calls: Mutex<Vec<(ItemId, QuestionRequest)>>,
    outcome: Mutex<Option<Result<QuestionOutcome, ElicitorError>>>,
}

impl RecordingElicitor {
    fn answered(answer: QuestionAnswer) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            outcome: Mutex::new(Some(Ok(QuestionOutcome::Answered(answer)))),
        }
    }
}

impl Elicitor for RecordingElicitor {
    fn elicit<'a>(
        &'a self,
        question: ItemId,
        request: QuestionRequest,
    ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>> {
        self.calls.lock().expect("elicitor calls").push((question, request));
        let outcome = self
            .outcome
            .lock()
            .expect("elicitor outcome")
            .take()
            .expect("one scripted elicitation");
        Box::pin(async move { outcome })
    }
}

#[test]
fn elicitor_receives_the_persisted_question_item_and_returns_a_typed_outcome() {
    let question = ItemId::new();
    let request = QuestionRequest::new(
        "Choose",
        QuestionMode::Single,
        vec![QuestionOption::new("yes", "Yes")],
        false,
    );
    let elicitor = RecordingElicitor::answered(QuestionAnswer::single("yes"));

    let outcome = futures::executor::block_on(elicitor.elicit(question, request.clone()))
        .expect("scripted answer");
    assert!(matches!(outcome, QuestionOutcome::Answered(_)));
    assert_eq!(elicitor.calls.lock().unwrap().as_slice(), &[(question, request)]);
}

// This is intentionally a compile RED until TurnLoop owns the single async
// Elicitor port. The full model-turn lifecycle will consume this same
// attachment once the runtime owner exists.
#[allow(dead_code)]
fn turn_loop_accepts_the_core_elicitor(
    turn: TurnLoop<'_>,
    elicitor: Arc<dyn Elicitor>,
) -> TurnLoop<'_> {
    turn.with_elicitor(elicitor)
}
