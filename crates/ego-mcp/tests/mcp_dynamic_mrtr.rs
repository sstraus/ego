//! Coarse contract for current dynamic MCP families and MRTR.

use std::collections::BTreeMap;

use ego_core::config::McpServerName;
use ego_core::generator::types::ToolDescriptor;
use ego_core::question::QuestionMode;
use ego_mcp::{McpCollapsedBootstrap, McpCollapsedCatalogue, McpInputResponse, McpInputState};
use rmcp::model::{ElicitRequest, ElicitRequestParams, InputRequest, InputRequiredResult};
use serde_json::json;

fn server() -> McpServerName {
    "alpha".parse().expect("server name")
}
fn schema() -> Vec<u8> {
    br#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#.to_vec()
}
fn original() -> Vec<u8> {
    br#"{"method":"tools/call","params":{"name":"call_tool","arguments":{"x":1}}}"#.to_vec()
}

fn descriptor(name: &str, parameters: serde_json::Value) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: "fixture".to_owned(),
        parameters,
    }
}

fn collapsed_management() -> Vec<ToolDescriptor> {
    vec![
        descriptor(
            "search_tools",
            json!({
                "type":"object",
                "properties":{"query":{"type":"string"},"limit":{"type":"integer","minimum":1}},
                "required":["query"],"additionalProperties":false
            }),
        ),
        descriptor(
            "get_tool_schema",
            json!({
                "type":"object",
                "properties":{"tool_name":{"type":"string"}},
                "required":["tool_name"],"additionalProperties":false
            }),
        ),
        descriptor(
            "call_tool",
            json!({
                "type":"object",
                "properties":{"tool_name":{"type":"string"},"arguments":{"type":"object"}},
                "required":["tool_name","arguments"],"additionalProperties":false
            }),
        ),
    ]
}

#[test]
fn collapsed_bootstrap_withholds_dispatch_until_exact_schema_lookup() {
    let bootstrap = McpCollapsedBootstrap::new(collapsed_management()).expect("bootstrap");
    assert_eq!(
        bootstrap.tool_names().collect::<Vec<_>>(),
        vec!["get_tool_schema", "search_tools"]
    );
    assert!(!bootstrap.tool_names().any(|name| name == "call_tool"));

    let structured_content = json!({
        "name":"weather",
        "description":"Current weather",
        "inputSchema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}
    });
    assert_eq!(
        structured_content,
        json!({"name":"weather","description":"Current weather","inputSchema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}})
    );
    let successor = McpCollapsedCatalogue::from_schema_response(
        bootstrap,
        "weather",
        json!({"structuredContent":structured_content}),
    )
    .expect("one exact schema lookup");
    assert!(successor.tool_names().any(|name| name == "call_tool"));
    assert!(successor.tool_names().any(|name| name == "weather"));
}

// Catches: optional MCP Tool metadata rejects an otherwise callable collapsed tool.
#[test]
fn collapsed_schema_with_annotations_and_optional_tool_fields_is_admitted() {
    let bootstrap = McpCollapsedBootstrap::new(collapsed_management()).expect("bootstrap");
    for metadata in [
        json!({"annotations":{"readOnlyHint":true}}),
        json!({"title":"Weather"}),
        json!({"outputSchema":{"type":"object"}}),
        json!({"_meta":{"example.com/source":"weather"}}),
        json!({
            "annotations":{"readOnlyHint":true,"destructiveHint":false},
            "title":"Weather",
            "outputSchema":{"type":"object"},
            "_meta":{"example.com/source":"weather"}
        }),
    ] {
        let mut definition = json!({
            "name":"weather",
            "description":"Current weather",
            "inputSchema":{"type":"object"}
        });
        definition
            .as_object_mut()
            .expect("definition")
            .extend(metadata.as_object().expect("metadata").clone());
        let catalogue = McpCollapsedCatalogue::from_schema_response(
            bootstrap,
            "weather",
            json!({"structuredContent":definition}),
        )
        .expect("optional MCP Tool fields do not prevent admission");
        assert_eq!(
            catalogue.tool_names().collect::<Vec<_>>(),
            ["call_tool", "weather"]
        );
    }
}

#[test]
fn collapsed_bootstrap_rejects_retired_alias_families_and_invalid_schema_results() {
    for names in [
        ["list_tools", "describe_tool", "execute_tool"],
        ["search_tools", "describe_tools", "execute_tool"],
    ] {
        let tools = names
            .into_iter()
            .map(|name| descriptor(name, json!({"type":"object","additionalProperties":false})))
            .collect();
        assert!(McpCollapsedBootstrap::new(tools).is_err());
    }

    let bootstrap = McpCollapsedBootstrap::new(collapsed_management()).expect("bootstrap");
    for result in [
        json!({"structuredContent":{}}),
        json!({"structuredContent":{"name":"search_tools","description":"recursive","inputSchema":{"type":"object"}}}),
        json!({"structuredContent":{"name":"weather","description":"extra","inputSchema":{"type":"object"},"extra":true}}),
        json!({"structuredContent":{"name":"weather","description":"duplicate","inputSchema":{"type":7}}}),
    ] {
        assert!(McpCollapsedCatalogue::from_schema_response(bootstrap, "weather", result).is_err());
    }
}

// Catches: accepting metadata accidentally bypasses required fields or schema validation.
#[test]
fn collapsed_schema_with_metadata_still_rejects_malformed_required_fields() {
    let bootstrap = McpCollapsedBootstrap::new(collapsed_management()).expect("bootstrap");
    let valid = json!({
        "name":"weather",
        "description":"Current weather",
        "inputSchema":{"type":"object"},
        "annotations":{"readOnlyHint":true}
    });
    for field in ["name", "description", "inputSchema"] {
        let mut missing = valid.clone();
        missing.as_object_mut().expect("definition").remove(field);
        assert!(
            McpCollapsedCatalogue::from_schema_response(
                bootstrap,
                "weather",
                json!({"structuredContent":missing}),
            )
            .is_err(),
            "missing {field}"
        );
        for malformed in [json!(null), json!(7), json!([])] {
            let mut definition = valid.clone();
            definition[field] = malformed;
            assert!(
                McpCollapsedCatalogue::from_schema_response(
                    bootstrap,
                    "weather",
                    json!({"structuredContent":definition}),
                )
                .is_err(),
                "malformed {field}"
            );
        }
    }
    for (field, value) in [
        ("name", json!("other")),
        ("name", json!("")),
        ("inputSchema", json!({"type":7})),
        ("extra", json!(true)),
    ] {
        let mut definition = valid.clone();
        definition[field] = value;
        assert!(
            McpCollapsedCatalogue::from_schema_response(
                bootstrap,
                "weather",
                json!({"structuredContent":definition}),
            )
            .is_err(),
            "invalid {field}"
        );
    }
}

#[test]
fn mrtr_requires_exact_manual_once_correlation_and_no_automatic_elicitor() {
    let state = McpInputState::required(server(), "tools/call", "call-7", schema(), original())
        .expect("input required");
    assert_eq!(state.method(), "input_required");
    let response = McpInputResponse::new(state.request_id(), br#"{"answer":"yes"}"#)
        .expect("validated answer");
    let retry = state.resume(response).expect("manual once retry");
    assert_eq!(retry.method(), "tools/call");
    assert_eq!(retry.call_id(), "call-7");
    assert!(
        std::str::from_utf8(retry.body())
            .expect("JSON")
            .contains("inputResponses")
    );
}

#[test]
fn mrtr_answer_is_single_use_and_mismatched_or_invalid_answers_never_retry() {
    let state = McpInputState::required(server(), "tools/call", "call-8", schema(), original())
        .expect("input required");
    let other = McpInputState::required(server(), "tools/call", "call-other", schema(), original())
        .expect("other input");
    let wrong = McpInputResponse::new(other.request_id(), br#"{"answer":"yes"}"#)
        .expect("valid other response");
    let request_id = state.request_id().clone();
    let response = McpInputResponse::new(&request_id, br#"{"answer":"yes"}"#).expect("answer");
    let retry = state.resume(response).expect("first claim");
    assert!(
        McpInputState::required(server(), "tools/call", "call-8", schema(), original())
            .expect("new state")
            .resume(wrong)
            .is_err()
    );
    assert!(McpInputResponse::new(&request_id, br#"{"answer":"yes"}"#).is_err());
    assert!(McpInputResponse::new(other.request_id(), br#"{"answer":"yes"}"#).is_ok());
    assert_eq!(retry.call_id(), "call-8");
}

#[test]
fn only_one_elicitation_form_is_mapped_and_other_input_required_forms_are_refused() {
    let elicitation = ego_mcp::McpInputRequired::elicitation(schema(), "call-10", br#"state"#)
        .expect("current elicitation form");
    assert!(ego_mcp::McpInputMapping::map(elicitation).is_ok());
    for unsupported in [
        ego_mcp::McpInputRequired::sampling(schema(), "call-10"),
        ego_mcp::McpInputRequired::roots(schema(), "call-10"),
        ego_mcp::McpInputRequired::multiple(schema(), "call-10"),
        ego_mcp::McpInputRequired::request_state_only(br#"state"#),
    ] {
        assert!(ego_mcp::McpInputMapping::map(unsupported).is_err());
    }
}

#[test]
fn rmcp_form_elicitation_retains_message_schema_and_continuation_authority() {
    let mut requests = BTreeMap::new();
    requests.insert(
        "human-answer".to_owned(),
        InputRequest::Elicitation(ElicitRequest::new(
            ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: "Confirm the deployment target".to_owned(),
                requested_schema: serde_json::from_value(json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "enum": ["staging", "production"]}
                    },
                    "required": ["target"]
                }))
                .expect("form schema"),
            },
        )),
    );
    let mapped = ego_mcp::McpInputMapping::from_rmcp(InputRequiredResult::new(
        Some(requests),
        Some("opaque-continuation".to_owned()),
    ))
    .expect("one accepted form elicitation");
    let question = mapped.question_request();

    assert_eq!(mapped.correlation_key(), "human-answer");
    assert_eq!(mapped.request_state(), b"opaque-continuation");
    assert_eq!(question.prompt, "Confirm the deployment target");
    assert_eq!(
        question.mode,
        QuestionMode::Form {
            schema: json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "enum": ["staging", "production"]}
                },
                "required": ["target"]
            })
        }
    );
    assert!(question.options.is_empty());
    assert!(!question.free_text);
    assert_eq!(
        mapped.schema(),
        &json!({
            "type": "object",
            "properties": {
                "target": {"type": "string", "enum": ["staging", "production"]}
            },
            "required": ["target"]
        })
    );
}
