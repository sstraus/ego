//! RED contract for ACP v1.5.0 unstable form elicitation.
//!
//! The imports below intentionally name the pinned v1 schema. In particular,
//! v1 has form modes and property schemas; it does not have fictional
//! `Select`, `MultiSelect`, or `Input` enum variants.

use std::collections::BTreeMap;

use agent_client_protocol_schema::v1::{
    ClientCapabilities, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, ElicitationFormMode, ElicitationMode,
    ElicitationCapabilities, ElicitationFormCapabilities,
    ElicitationPropertySchema, ElicitationRequestScope, ElicitationSchema,
    MultiSelectPropertySchema, RequestId, StringPropertySchema,
};
use ego_acp::question::{
    AcpQuestionAdapter, CapabilityCapture, QuestionMappingError, QuestionRequest,
};
use ego_core::event::id::ItemId;
use ego_core::question::{QuestionMode, QuestionOption};

fn single_request() -> QuestionRequest {
    QuestionRequest::new(
        "Choose a colour",
        QuestionMode::Single,
        vec![QuestionOption::new("red", "Red"), QuestionOption::new("blue", "Blue")],
        false,
    )
}

fn multiple_request() -> QuestionRequest {
    QuestionRequest::new(
        "Choose colours",
        QuestionMode::Multiple,
        vec![QuestionOption::new("red", "Red"), QuestionOption::new("blue", "Blue")],
        false,
    )
}

fn free_text_request() -> QuestionRequest {
    QuestionRequest::new("Explain", QuestionMode::FreeText, vec![], true)
}

fn scope() -> ElicitationRequestScope {
    ElicitationRequestScope::new(RequestId::Number(7))
}

#[test]
fn single_maps_to_a_v1_form_string_enum() {
    let request = AcpQuestionAdapter::form_request(ItemId::new(), &single_request()).unwrap();
    let CreateElicitationRequest {
        mode: ElicitationMode::Form(ElicitationFormMode { requested_schema, .. }),
        ..
    } = request
    else {
        panic!("single questions use v1 form mode");
    };
    assert!(matches!(
        requested_schema.properties.get("answer"),
        Some(ElicitationPropertySchema::String(StringPropertySchema {
            enum_values: Some(values), ..
        })) if values == &["red".to_string(), "blue".to_string()]
    ));
}

#[test]
fn multiple_maps_to_a_v1_form_multi_select_array() {
    let request = AcpQuestionAdapter::form_request(ItemId::new(), &multiple_request()).unwrap();
    let ElicitationMode::Form(form) = request.mode else {
        panic!("multiple questions use v1 form mode");
    };
    assert!(matches!(
        form.requested_schema.properties.get("answers"),
        Some(ElicitationPropertySchema::Array(MultiSelectPropertySchema { .. }))
    ));
}

#[test]
fn free_text_maps_to_a_v1_form_string_property_without_options() {
    let request = AcpQuestionAdapter::form_request(ItemId::new(), &free_text_request()).unwrap();
    let ElicitationMode::Form(form) = request.mode else {
        panic!("free text questions use v1 form mode");
    };
    assert!(matches!(
        form.requested_schema.properties.get("answer"),
        Some(ElicitationPropertySchema::String(StringPropertySchema {
            enum_values: None,
            one_of: None,
            ..
        }))
    ));
}

#[test]
fn adapter_captures_initialize_form_capability_and_uses_the_real_scope_shape() {
    let mut capabilities = CapabilityCapture::default();
    assert!(!capabilities.form_available());
    let mut initialize = ClientCapabilities::default();
    initialize.elicitation = Some(
        ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
    );
    capabilities.capture_initialize(&initialize);
    assert!(capabilities.form_available());

    let request = CreateElicitationRequest::new(
        ElicitationFormMode::new(scope(), ElicitationSchema::new().string("answer", true)),
        "Choose",
    );
    assert!(matches!(request.mode, ElicitationMode::Form(_)));
    assert!(matches!(request.scope(), _));
}

#[test]
fn accepted_content_is_revalidated_by_core_before_settlement() {
    let question = ItemId::new();
    let valid = CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(BTreeMap::from([(
            "answer".into(),
            ElicitationContentValue::from("red"),
        )])),
    ));
    assert!(AcpQuestionAdapter::validate_response(question, &single_request(), &valid).is_ok());

    let invalid = CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(BTreeMap::from([(
            "answer".into(),
            ElicitationContentValue::from("unknown"),
        )])),
    ));
    assert!(matches!(
        AcpQuestionAdapter::validate_response(question, &single_request(), &invalid),
        Err(QuestionMappingError::InvalidAnswer { .. })
    ));
}

#[test]
fn multiple_response_uses_string_array_and_wrong_content_type_stays_pending() {
    let question = ItemId::new();
    let valid = CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(BTreeMap::from([(
            "answers".into(),
            ElicitationContentValue::from(vec!["red", "blue"]),
        )])),
    ));
    assert!(AcpQuestionAdapter::validate_response(question, &multiple_request(), &valid).is_ok());

    let wrong = CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(BTreeMap::from([(
            "answers".into(),
            ElicitationContentValue::from("red"),
        )])),
    ));
    assert!(AcpQuestionAdapter::validate_response(question, &multiple_request(), &wrong).is_err());
}

#[test]
fn missing_form_capability_returns_stable_id_and_resume_guidance_without_settling() {
    let question = ItemId::new();
    let adapter = AcpQuestionAdapter::new(CapabilityCapture::default());
    let error = adapter
        .elicit(question, &single_request())
        .expect_err("missing capability must not hang or infer");
    assert!(matches!(error, QuestionMappingError::CapabilityUnavailable { id, .. } if id == question));
    assert!(error.to_string().contains("ego resume"));
    assert!(error.to_string().contains(&question.to_string()));
    assert!(adapter.pending(question));
}

#[test]
fn decline_and_cancel_are_distinct_failed_settlements() {
    let question = ItemId::new();
    for action in [ElicitationAction::Decline, ElicitationAction::Cancel] {
        let response = CreateElicitationResponse::new(action);
        let adapter = AcpQuestionAdapter::with_response(response);
        let result = adapter.elicit(question, &free_text_request()).unwrap();
        assert!(result.is_declined() || result.is_cancelled());
        assert!(!adapter.pending(question));
        assert!(result.tool_result().is_pure());
        assert!(result.tool_result().is_failure());
    }
}

#[test]
fn process_and_session_cancellation_close_the_session_not_an_unrelated_question() {
    for process in [true, false] {
        let adapter = AcpQuestionAdapter::new(CapabilityCapture::form());
        let question = ItemId::new();
        let outcome = if process {
            adapter.cancel_process(question)
        } else {
            adapter.cancel_session(question)
        };
        assert!(outcome.closes_sibling_batch());
        assert!(outcome.closes_turn());
        assert!(!adapter.pending(question));
    }
}
