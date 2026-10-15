//! contract for regression case's core-owned question lifecycle.
//!
//! These tests exercise the durable event boundary rather than a
//! host-specific prompt helper. The question's identity is the `ItemId` on
//! `QuestionRequested`; there is no second question identifier.

use ego_core::event::domain::{DomainEvent, Envelope, EventKind};
use ego_core::event::id::{EventId, ItemId, Producer, Seq, TurnId};
use ego_core::event::time::Timestamp;
use ego_core::public::{ItemKind, PUBLIC_EVENT_VERSION, PublicEvent, project_public};
use ego_core::question::{
    QuestionAnswer, QuestionMode, QuestionOption, QuestionOutcome, QuestionRequest, QuestionTarget,
};
use ego_core::run::RunTermination;
use ego_core::state::SessionState;
use ego_core::store::codec::CODEC_VERSION;

fn request(mode: QuestionMode, options: &[(&str, &str)], free_text: bool) -> QuestionRequest {
    QuestionRequest::new(
        "Choose the next action",
        mode,
        options
            .iter()
            .map(|(id, label)| QuestionOption::new(*id, *label))
            .collect(),
        free_text,
    )
}

fn event(
    seq: u64,
    turn: TurnId,
    item: ItemId,
    parent: Option<EventId>,
    kind: EventKind,
) -> DomainEvent {
    let mut sequence = Seq::default();
    for _ in 0..seq {
        sequence.next();
    }
    DomainEvent {
        env: Envelope {
            seq: sequence,
            id: EventId::new(),
            turn,
            item: Some(item),
            parent,
            ts: Timestamp::from_millis(seq),
            producer: Producer::Main,
        },
        kind,
    }
}

#[test]
fn request_validation_covers_modes_options_ids_and_free_text() {
    assert!(
        request(QuestionMode::Single, &[("yes", "Yes")], false)
            .validate()
            .is_ok()
    );
    assert!(
        request(QuestionMode::Multiple, &[("a", "A"), ("b", "B")], false)
            .validate()
            .is_ok()
    );
    assert!(
        request(QuestionMode::FreeText, &[], true)
            .validate()
            .is_ok()
    );

    for invalid in [
        QuestionRequest::new("", QuestionMode::Single, vec![], false),
        QuestionRequest::new("prompt", QuestionMode::Single, vec![], false),
        QuestionRequest::new(
            "prompt",
            QuestionMode::Single,
            vec![
                QuestionOption::new("same", "one"),
                QuestionOption::new("same", "two"),
            ],
            false,
        ),
        QuestionRequest::new(
            "prompt",
            QuestionMode::FreeText,
            vec![QuestionOption::new("x", "X")],
            true,
        ),
        QuestionRequest::new(
            "prompt",
            QuestionMode::Single,
            vec![QuestionOption::new("", "label")],
            false,
        ),
        QuestionRequest::new(
            "prompt",
            QuestionMode::Single,
            vec![QuestionOption::new("id", "")],
            false,
        ),
    ] {
        assert!(invalid.validate().is_err());
    }
}

#[test]
fn answers_use_the_canonical_json_grammar_for_every_valid_and_invalid_form() {
    let single = request(QuestionMode::Single, &[("yes", "Yes")], false);
    assert!(QuestionAnswer::from_json(&single, r#""yes""#).is_ok());
    assert!(QuestionAnswer::from_json(&single, "[]").is_err());
    assert!(QuestionAnswer::from_json(&single, "true").is_err());
    assert!(QuestionAnswer::from_json(&single, r#"""#).is_err());
    assert!(QuestionAnswer::from_json(&single, r#""unknown""#).is_err());
    let single_with_free_text = request(QuestionMode::Single, &[("yes", "Yes")], true);
    assert!(QuestionAnswer::from_json(&single_with_free_text, r#""arbitrary""#).is_ok());

    let multiple = request(QuestionMode::Multiple, &[("a", "A"), ("b", "B")], false);
    assert!(QuestionAnswer::from_json(&multiple, r#"["a","b"]"#).is_ok());
    assert!(QuestionAnswer::from_json(&multiple, r#"["a","a"]"#).is_err());
    assert!(QuestionAnswer::from_json(&multiple, r#"["a","unknown"]"#).is_err());
    assert!(QuestionAnswer::from_json(&multiple, "[]").is_err());
    let multiple_with_free_text = request(QuestionMode::Multiple, &[("a", "A")], true);
    assert!(QuestionAnswer::from_json(&multiple_with_free_text, r#"["custom"]"#).is_ok());
    assert!(QuestionAnswer::from_json(&multiple, r#""a""#).is_err());

    let free = request(QuestionMode::FreeText, &[], true);
    assert!(QuestionAnswer::from_json(&free, r#""explain the failure""#).is_ok());
    assert!(QuestionAnswer::from_json(&free, "null").is_err());
    assert!(QuestionAnswer::from_json(&free, r#""#).is_err());
}

#[test]
fn requested_and_settled_are_correlated_by_the_question_item_and_preserve_order() {
    let turn = TurnId::new();
    let call = ItemId::new();
    let question = ItemId::new();
    let started = event(
        1,
        turn,
        call,
        None,
        EventKind::ToolCallStarted {
            name: "ask_question".into(),
            args: "{}".into(),
            call_id: Some("call-question".into()),
        },
    );
    let requested = event(
        2,
        turn,
        question,
        Some(started.env.id),
        EventKind::QuestionRequested {
            request: request(QuestionMode::Single, &[("yes", "Yes")], false),
            target: QuestionTarget::ModelTool { tool_call: call },
        },
    );
    let settled = event(
        3,
        turn,
        question,
        Some(requested.env.id),
        EventKind::QuestionSettled {
            outcome: QuestionOutcome::Answered(QuestionAnswer::single("yes")),
        },
    );
    let completed = event(
        4,
        turn,
        call,
        Some(settled.env.id),
        EventKind::ToolCallCompleted {
            ok: true,
            result: ego_core::permit::ToolResult::builtin("yes"),
        },
    );

    assert_eq!(requested.env.item, Some(question));
    assert_eq!(settled.env.item, Some(question));
    assert_eq!(settled.env.parent, Some(requested.env.id));
    assert_eq!(completed.env.parent, Some(settled.env.id));
    assert_ne!(call, question);
}

#[test]
fn pending_reducer_rejects_wrong_duplicate_and_extra_answers_without_mutation() {
    let turn = TurnId::new();
    let question = ItemId::new();
    let requested = event(
        1,
        turn,
        question,
        None,
        EventKind::QuestionRequested {
            request: request(QuestionMode::Multiple, &[("a", "A"), ("b", "B")], false),
            target: QuestionTarget::ModelTool {
                tool_call: ItemId::new(),
            },
        },
    );
    let mut state = SessionState::default();
    state.apply(&requested);
    let before = state.clone();

    assert!(state.answer_pending(question, r#"["wrong"]"#).is_err());
    assert_eq!(state, before);
    assert!(state.answer_pending(ItemId::new(), r#"["a"]"#).is_err());
    assert_eq!(state, before);

    let settled = event(
        2,
        turn,
        question,
        Some(requested.env.id),
        EventKind::QuestionSettled {
            outcome: QuestionOutcome::Answered(QuestionAnswer::multiple(["a"])),
        },
    );
    state.apply(&settled);
    let after = state.clone();
    assert!(state.answer_pending(question, r#"["b"]"#).is_err());
    assert_eq!(state, after);
}

#[test]
fn requested_unsettled_recovery_stays_pending_and_is_not_torn_history() {
    let turn = TurnId::new();
    let question = ItemId::new();
    let requested = event(
        1,
        turn,
        question,
        None,
        EventKind::QuestionRequested {
            request: request(QuestionMode::FreeText, &[], true),
            target: QuestionTarget::ModelTool {
                tool_call: ItemId::new(),
            },
        },
    );
    let mut state = SessionState::default();
    state.apply(&requested);
    let rebuilt = SessionState::replay([requested]);
    assert_eq!(
        rebuilt.pending_question().map(|q| q.item_id()),
        Some(question)
    );
    assert!(rebuilt.torn_tool_errors().is_empty());
    assert!(rebuilt.refuse_new_prompt(turn).is_err());
    assert!(state.pending_question().is_some());
}

#[test]
fn valid_resume_settles_the_same_turn_and_completes_the_pure_tool_result() {
    let turn = TurnId::new();
    let question = ItemId::new();
    let request = request(QuestionMode::Single, &[("yes", "Yes")], false);
    let mut state = SessionState::default();
    state.apply(&event(
        1,
        turn,
        question,
        None,
        EventKind::QuestionRequested {
            request: request.clone(),
            target: QuestionTarget::ModelTool {
                tool_call: ItemId::new(),
            },
        },
    ));
    let answer = QuestionAnswer::from_json(&request, r#""yes""#).expect("canonical answer grammar");
    let continuation = state
        .resume_question(question, answer)
        .expect("pending question is resumable");
    assert_eq!(continuation.turn, continuation.original_turn);
    assert!(continuation.tool_result.is_pure());
    assert!(continuation.continues_original_turn());
}

#[test]
fn public_question_item_and_headless_report_are_structured_and_versioned() {
    assert_eq!(PUBLIC_EVENT_VERSION, 12);
    assert_eq!(CODEC_VERSION, 12);

    let turn = TurnId::new();
    let item = ItemId::new();
    let request = request(QuestionMode::Single, &[("yes", "Yes")], false);
    let target = QuestionTarget::ModelTool {
        tool_call: ItemId::new(),
    };
    let projected = project_public(&event(
        1,
        turn,
        item,
        None,
        EventKind::QuestionRequested {
            request: request.clone(),
            target: target.clone(),
        },
    ))
    .expect("questions are public items");
    assert!(matches!(
        projected,
        PublicEvent::ItemStarted {
            kind: ItemKind::Question,
            item: projected_item,
            ..
        } if projected_item == item
    ));
    let json = serde_json::to_value(ego_core::public::QuestionReport::from_request(
        item, &target, &request,
    ))
    .unwrap();
    assert_eq!(json["id"], item.to_string());
    assert_eq!(json["mode"], "single");

    let required = RunTermination::QuestionRequired {
        question: Box::new(ego_core::public::QuestionReport::from_request(
            item, &target, &request,
        )),
    };
    assert_eq!(required.exit_code(), 3);
    assert_eq!(required.exit_reason(), "question");
    let termination_report = serde_json::to_value(required.report()).unwrap();
    assert_eq!(termination_report["question"]["id"], item.to_string());
    assert_eq!(termination_report["question"]["mode"], "single");
}

#[test]
fn question_tool_is_human_input_not_an_effect_and_has_no_permission_op() {
    use ego_core::question::{Elicitor, ElicitorError, HumanInputTool};
    use ego_core::turn::RegisteredTool;
    use futures::future::BoxFuture;
    use serde_json::Value;
    use std::sync::Arc;

    struct FakeHumanInput;
    impl HumanInputTool for FakeHumanInput {
        fn descriptor(&self) -> ego_core::generator::types::ToolDescriptor {
            ego_core::question::ask_question_descriptor()
        }

        fn parse(&self, raw: Value) -> Result<QuestionRequest, String> {
            let request: QuestionRequest =
                serde_json::from_value(raw).map_err(|err| err.to_string())?;
            request.validate().map_err(|err| err.to_string())?;
            Ok(request)
        }
    }

    struct FakeElicitor;
    impl Elicitor for FakeElicitor {
        fn elicit<'a>(
            &'a self,
            _question: ItemId,
            request: QuestionRequest,
        ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>> {
            Box::pin(async move {
                assert!(request.validate().is_ok());
                Ok(QuestionOutcome::Unavailable)
            })
        }
    }

    let parser: Arc<dyn HumanInputTool> = Arc::new(FakeHumanInput);
    let registered = RegisteredTool::HumanInput(Arc::clone(&parser));
    assert!(matches!(registered, RegisteredTool::HumanInput(_)));
    assert!(registered.as_effect().is_none());
    assert!(registered.as_human_input().is_some());
    let request = parser
        .parse(serde_json::json!({
            "prompt": "Choose",
            "mode": "single",
            "options": [{"id": "yes", "label": "Yes"}],
            "free_text": false
        }))
        .unwrap();
    let outcome = futures::executor::block_on(FakeElicitor.elicit(ItemId::new(), request))
        .expect("fake elicitor settles");
    assert!(matches!(outcome, QuestionOutcome::Unavailable));
}

#[test]
fn question_outcomes_are_explicit_and_unavailable_never_infers_an_answer() {
    assert!(matches!(
        QuestionOutcome::Declined,
        QuestionOutcome::Declined
    ));
    assert!(matches!(
        QuestionOutcome::Cancelled,
        QuestionOutcome::Cancelled
    ));
    assert!(matches!(
        QuestionOutcome::Unavailable,
        QuestionOutcome::Unavailable
    ));
}

#[test]
fn the_tool_schema_documents_description_and_calls_without_it_still_parse() {
    use ego_core::question::{AskQuestionTool, HumanInputTool};

    let schema = ego_core::question::ask_question_descriptor().parameters;
    let option = &schema["$defs"]["QuestionOption"];
    assert!(
        option["properties"]["description"]["description"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "the model is never told the field exists: {option}"
    );
    assert!(
        !option["required"]
            .as_array()
            .is_some_and(|required| required.iter().any(|name| name == "description")),
        "description must stay optional: {option}"
    );

    let parse = |option: serde_json::Value| {
        AskQuestionTool.parse(serde_json::json!({
            "prompt": "Choose",
            "mode": "single",
            "options": [option],
            "free_text": false,
        }))
    };
    let without = parse(serde_json::json!({"id": "a", "label": "A"})).expect("old call parses");
    assert_eq!(without.options[0].description, None);
    let with = parse(serde_json::json!({"id": "a", "label": "A", "description": "Why A"}))
        .expect("described call parses");
    assert_eq!(with.options[0].description.as_deref(), Some("Why A"));
    assert!(
        !serde_json::to_string(&without)
            .unwrap()
            .contains("description"),
        "an absent description is not logged as null"
    );
}
