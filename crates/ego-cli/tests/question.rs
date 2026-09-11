//! contract for interactive Form answers through the canonical core parser.

use ego_core::question::{QuestionAnswer, QuestionMode, QuestionRequest};
use serde_json::json;

fn form_request() -> QuestionRequest {
    QuestionRequest::new(
        "Describe the release",
        QuestionMode::Form {
            schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "count": { "type": "integer", "minimum": 1 }
                },
                "required": ["name", "count"],
                "additionalProperties": false
            }),
        },
        Vec::new(),
        false,
    )
}

#[test]
fn interactive_form_json_object_uses_canonical_answer_validation() {
    let request = form_request();
    let raw = r#"{"name":"v10","count":2}"#;
    let answer = QuestionAnswer::from_json(&request, raw).expect("valid Form object");

    assert_eq!(
        answer,
        QuestionAnswer::form(
            [
                ("name".to_owned(), json!("v10")),
                ("count".to_owned(), json!(2))
            ]
            .into_iter()
            .collect()
        )
    );
    answer
        .validate_for(&request)
        .expect("canonical Form answer validates against its request");
}

#[test]
fn interactive_form_rejects_non_object_and_schema_invalid_json() {
    let request = form_request();

    assert!(QuestionAnswer::from_json(&request, r#"["v10"]"#).is_err());
    assert!(QuestionAnswer::from_json(&request, r#"{"name":"v10"}"#).is_err());
    assert!(QuestionAnswer::from_json(&request, "not-json").is_err());
}
