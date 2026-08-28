//! Coarse RED contract for current dynamic MCP families and MRTR.

use ego_core::config::McpServerName;
use ego_core::generator::types::ToolDescriptor;
use ego_mcp::{McpDynamicCatalogue, McpDynamicFamily, McpInputResponse, McpInputState};
use serde_json::json;

fn server() -> McpServerName {
    "alpha".parse().expect("server name")
}
fn schema() -> Vec<u8> {
    br#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#.to_vec()
}
fn original() -> Vec<u8> {
    br#"{"method":"tools/call","params":{"name":"execute_tool","arguments":{"x":1}}}"#.to_vec()
}

fn descriptor(name: &str, parameters: serde_json::Value) -> ToolDescriptor {
    ToolDescriptor {
        name: name.to_owned(),
        description: "fixture".to_owned(),
        parameters,
    }
}

#[test]
fn standalone_speakeasy_outer_and_inner_authorities_are_independent() {
    let tools = vec![
        descriptor(
            "list_tools",
            json!({"type":"object","additionalProperties":false}),
        ),
        descriptor(
            "describe_tool",
            json!({"type":"object","additionalProperties":false}),
        ),
        descriptor(
            "execute_tool",
            json!({"type":"object","properties":{"name":{"type":"string"},"arguments":{"type":"object"}},"required":["name","arguments"],"additionalProperties":false}),
        ),
    ];
    let family = McpDynamicFamily::detect(&tools)
        .expect("family")
        .expect("standalone");
    assert_eq!(family, McpDynamicFamily::standalone());
    let catalogue = McpDynamicCatalogue::new(family, vec![descriptor(
        "inner",
        json!({"type":"object","properties":{"x":{"type":"integer"}},"required":["x"],"additionalProperties":false}),
    )]).expect("compiled inner schema");
    assert_eq!(catalogue.tool_names().collect::<Vec<_>>(), vec!["inner"]);
    assert!(catalogue.tool_names().any(|name| name == "inner"));
}

#[test]
fn gram_dynamic_toolsets_require_current_inner_schema() {
    let tools = vec![
        descriptor(
            "search_tools",
            json!({"type":"object","additionalProperties":false}),
        ),
        descriptor(
            "describe_tools",
            json!({"type":"object","additionalProperties":false}),
        ),
        descriptor(
            "execute_tool",
            json!({"type":"object","properties":{"name":{"type":"string"},"arguments":{"type":"object"}},"required":["name","arguments"],"additionalProperties":false}),
        ),
    ];
    let family = McpDynamicFamily::detect(&tools)
        .expect("family")
        .expect("Gram");
    assert_eq!(family, McpDynamicFamily::gram());
    let catalogue = McpDynamicCatalogue::new(family, vec![descriptor(
        "weather",
        json!({"type":"object","properties":{"city":{"type":"string"}},"required":["city"],"additionalProperties":false}),
    )]).expect("schema");
    assert!(catalogue.tool_names().any(|name| name == "weather"));
    assert!(!catalogue.tool_names().any(|name| name == "missing"));
    assert!(
        McpDynamicCatalogue::new(
            McpDynamicFamily::gram(),
            vec![descriptor("broken", json!({"type": 7}))],
        )
        .is_err()
    );
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
