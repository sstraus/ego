//! RED contract for ACP v1.5.0 unstable form elicitation.
//!
//! The imports below intentionally name the pinned v1 schema. In particular,
//! v1 has form modes and property schemas; it does not have fictional
//! `Select`, `MultiSelect`, or `Input` enum variants.

use futures::executor::block_on;
use std::collections::BTreeMap;

use agent_client_protocol_schema::v1::{
    BooleanPropertySchema, ClientCapabilities, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAcceptAction, ElicitationAction, ElicitationCapabilities, ElicitationContentValue,
    ElicitationFormCapabilities, ElicitationFormMode, ElicitationMode, ElicitationPropertySchema,
    ElicitationScope, IntegerPropertySchema, MultiSelectPropertySchema, NumberPropertySchema,
    SessionId, StringPropertySchema,
};
use ego_acp::question::{
    AcpQuestionAdapter, CapabilityCapture, QuestionMappingError, QuestionRequest,
};
use ego_core::event::id::ItemId;
use ego_core::question::{QuestionAnswer, QuestionMode, QuestionOption, QuestionOutcome};
use serde_json::json;

fn single_request() -> QuestionRequest {
    QuestionRequest::new(
        "Choose a colour",
        QuestionMode::Single,
        vec![
            QuestionOption::new("red", "Red"),
            QuestionOption::new("blue", "Blue"),
        ],
        false,
    )
}

fn multiple_request() -> QuestionRequest {
    QuestionRequest::new(
        "Choose colours",
        QuestionMode::Multiple,
        vec![
            QuestionOption::new("red", "Red"),
            QuestionOption::new("blue", "Blue"),
        ],
        false,
    )
}

fn free_text_request() -> QuestionRequest {
    QuestionRequest::new("Explain", QuestionMode::FreeText, vec![], true)
}

fn form_request() -> QuestionRequest {
    QuestionRequest::new(
        "Configure deployment",
        QuestionMode::Form {
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "minLength": 2,
                        "maxLength": 40
                    },
                    "count": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 9
                    },
                    "ratio": {
                        "type": "number",
                        "minimum": 0.5,
                        "maximum": 1.0
                    },
                    "enabled": {"type": "boolean"},
                    "tags": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": ["alpha", "beta", "gamma"]
                        },
                        "minItems": 1,
                        "maxItems": 2
                    }
                },
                "required": ["name", "count", "ratio", "enabled", "tags"]
            }),
        },
        vec![],
        false,
    )
}

fn session() -> SessionId {
    SessionId::new("01932d5e-0000-7000-8000-0000000000aa")
}

#[test]
fn single_maps_to_a_v1_form_string_enum() {
    let request =
        AcpQuestionAdapter::form_request(&session(), ItemId::new(), &single_request()).unwrap();
    let CreateElicitationRequest {
        mode:
            ElicitationMode::Form(ElicitationFormMode {
                requested_schema, ..
            }),
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
    let request =
        AcpQuestionAdapter::form_request(&session(), ItemId::new(), &multiple_request()).unwrap();
    let ElicitationMode::Form(form) = request.mode else {
        panic!("multiple questions use v1 form mode");
    };
    assert!(matches!(
        form.requested_schema.properties.get("answers"),
        Some(ElicitationPropertySchema::Array(
            MultiSelectPropertySchema { .. }
        ))
    ));
}

#[test]
fn free_text_maps_to_a_v1_form_string_property_without_options() {
    let request =
        AcpQuestionAdapter::form_request(&session(), ItemId::new(), &free_text_request()).unwrap();
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
fn form_maps_the_primitive_object_schema_without_wrapping_or_renaming() {
    let request =
        AcpQuestionAdapter::form_request(&session(), ItemId::new(), &form_request()).unwrap();
    let CreateElicitationRequest {
        mode:
            ElicitationMode::Form(ElicitationFormMode {
                requested_schema, ..
            }),
        message,
        ..
    } = request
    else {
        panic!("form questions use v1 form mode");
    };

    assert_eq!(message, "Configure deployment");
    assert_eq!(
        serde_json::to_value(requested_schema).expect("schema is serializable"),
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "minLength": 2,
                    "maxLength": 40
                },
                "count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 9
                },
                "ratio": {
                    "type": "number",
                    "minimum": 0.5,
                    "maximum": 1.0
                },
                "enabled": {"type": "boolean"},
                "tags": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": ["alpha", "beta", "gamma"]
                    },
                    "minItems": 1,
                    "maxItems": 2
                }
            },
            "required": ["name", "count", "ratio", "enabled", "tags"]
        })
    );

    // Keep the public property variants explicit so a projection cannot pass
    // by serializing a differently typed or renamed field.
    let request =
        AcpQuestionAdapter::form_request(&session(), ItemId::new(), &form_request()).unwrap();
    let ElicitationMode::Form(form) = request.mode else {
        panic!("form questions use v1 form mode");
    };
    assert!(matches!(
        form.requested_schema.properties.get("name"),
        Some(ElicitationPropertySchema::String(StringPropertySchema {
            min_length: Some(2),
            max_length: Some(40),
            ..
        }))
    ));
    assert!(matches!(
        form.requested_schema.properties.get("count"),
        Some(ElicitationPropertySchema::Integer(IntegerPropertySchema {
            minimum: Some(1),
            maximum: Some(9),
            ..
        }))
    ));
    assert!(matches!(
        form.requested_schema.properties.get("ratio"),
        Some(ElicitationPropertySchema::Number(NumberPropertySchema {
            minimum: Some(minimum),
            maximum: Some(maximum),
            ..
        })) if *minimum == 0.5 && *maximum == 1.0
    ));
    assert!(matches!(
        form.requested_schema.properties.get("enabled"),
        Some(ElicitationPropertySchema::Boolean(
            BooleanPropertySchema { .. }
        ))
    ));
    assert!(matches!(
        form.requested_schema.properties.get("tags"),
        Some(ElicitationPropertySchema::Array(
            MultiSelectPropertySchema {
                min_items: Some(1),
                max_items: Some(2),
                ..
            }
        ))
    ));
}

#[test]
fn accepted_form_content_becomes_the_same_validated_core_object() {
    let question = ItemId::new();
    let request = form_request();
    let response = CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(BTreeMap::from([
            ("name".into(), ElicitationContentValue::from("production")),
            ("count".into(), ElicitationContentValue::from(3_i64)),
            ("ratio".into(), ElicitationContentValue::from(0.75_f64)),
            ("enabled".into(), ElicitationContentValue::from(true)),
            (
                "tags".into(),
                ElicitationContentValue::from(vec!["alpha", "beta"]),
            ),
        ])),
    ));

    let outcome = AcpQuestionAdapter::validate_response(question, &request, &response)
        .expect("valid ACP form content becomes a core answer");
    let QuestionOutcome::Answered(QuestionAnswer::Form(answer)) = outcome else {
        panic!("accepted form content must become QuestionAnswer::Form");
    };
    let expected = json!({
        "name": "production",
        "count": 3,
        "ratio": 0.75,
        "enabled": true,
        "tags": ["alpha", "beta"]
    })
    .as_object()
    .expect("object answer")
    .clone();
    assert_eq!(answer, expected);
    assert!(
        QuestionAnswer::Form(answer).validate_for(&request).is_ok(),
        "the mapped object must pass the original durable schema"
    );
}

#[test]
fn schema_invalid_accepted_form_content_fails_before_settlement() {
    let question = ItemId::new();
    let request = form_request();
    let response = CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(BTreeMap::from([
            ("name".into(), ElicitationContentValue::from("production")),
            // The durable schema requires an integer; ACP's string value must
            // not be coerced before QuestionAnswer validation.
            ("count".into(), ElicitationContentValue::from("three")),
            ("ratio".into(), ElicitationContentValue::from(0.75_f64)),
            ("enabled".into(), ElicitationContentValue::from(true)),
            ("tags".into(), ElicitationContentValue::from(vec!["alpha"])),
        ])),
    ));

    assert!(matches!(
        AcpQuestionAdapter::validate_response(question, &request, &response),
        Err(QuestionMappingError::InvalidAnswer { .. })
    ));
}

#[test]
fn adapter_captures_initialize_form_capability_and_uses_the_real_scope_shape() {
    let mut capabilities = CapabilityCapture::default();
    assert!(!capabilities.form_available());
    let mut initialize = ClientCapabilities::default();
    initialize.elicitation =
        Some(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
    capabilities.capture_initialize(&initialize);
    assert!(capabilities.form_available());

    // Built through the adapter and not by hand: what is under test is the
    // shape ego actually puts on the wire. The previous assertion here read
    // `matches!(request.scope(), _)`, which every possible value satisfies,
    // and it sat under a name claiming the real scope shape was checked.
    let request = AcpQuestionAdapter::form_request(&session(), ItemId::new(), &single_request())
        .expect("a valid single question maps");
    assert!(matches!(request.mode, ElicitationMode::Form(_)));
    let ElicitationScope::Session(scope) = request.scope() else {
        panic!("a question asked inside a session is session-scoped: {request:?}");
    };
    assert_eq!(scope.session_id, session());
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
fn multiple_response_uses_string_array_and_wrong_content_type_stays_invalid() {
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
    let adapter = AcpQuestionAdapter::new(session(), CapabilityCapture::default());
    let outcome = block_on(adapter.elicitor().elicit(question, single_request()))
        .expect("missing capability is an unavailable question outcome");
    assert_eq!(outcome, QuestionOutcome::Unavailable);
}

#[test]
fn decline_and_cancel_are_distinct_failed_settlements() {
    let question = ItemId::new();
    for action in [ElicitationAction::Decline, ElicitationAction::Cancel] {
        let response = CreateElicitationResponse::new(action);
        let result =
            AcpQuestionAdapter::validate_response(question, &free_text_request(), &response)
                .unwrap();
        assert!(matches!(
            result,
            QuestionOutcome::Declined | QuestionOutcome::Cancelled
        ));
    }
}
