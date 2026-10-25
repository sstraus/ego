mod support;

use ego_core::mcp::{McpAdmissionFingerprint, McpOperationKind, McpSubject};
use ego_core::permit::judge::JudgeTaskContext;
use ego_core::permit::{ForeignToolId, InvocationDigest, PreparedCall};
use serde_json::json;

fn mcp_call(parameters: serde_json::Value) -> PreparedCall {
    PreparedCall::mcp_acp(
        McpSubject::Tool {
            server: "tuic".into(),
            name: "call_tool".into(),
        },
        McpOperationKind::CallTool,
        vec![(
            ForeignToolId::new("tuic", "call_tool").unwrap(),
            InvocationDigest::from_bytes(&serde_json::to_vec(&parameters).unwrap()),
        )],
        McpAdmissionFingerprint::new("capture", "call_tool", "schema").unwrap(),
        "transport",
        "tuic",
        &parameters,
    )
    .unwrap()
}

// Catches: read-only MCP lookups expose only a digest, forcing the judge to escalate.
#[test]
fn auto_judge_mcp_parameters_are_evidence_not_only_digest() {
    let parameters =
        json!({"tool_name":"agent", "arguments":{"action":"list", "token":"private-value"}});
    let call = mcp_call(parameters);
    let request = support::judge_request(&call, &JudgeTaskContext::default());
    let text = serde_json::to_string(&request.messages).unwrap();
    assert!(text.contains("agent"), "MCP target missing: {text}");
    assert!(text.contains("list"), "MCP effect missing: {text}");
    assert!(!text.contains("private-value"), "secret leaked: {text}");
}

// Catches: nested credentials or free-form annotations leak to either judge provider.
#[test]
fn mcp_evidence_redacts_nested_secrets_and_retains_only_boolean_hints() {
    let call = mcp_call(json!({"arguments":{"api_key":"private-value", "rows":[{"password":"private-password"}], "action":"list"}}))
        .with_mcp_annotations(json!({"agent":{"readOnlyHint":true, "destructiveHint":false, "title":"private-title"}}));
    let text = serde_json::to_string(
        &support::judge_request(&call, &JudgeTaskContext::default()).messages,
    )
    .unwrap();
    for secret in ["private-value", "private-password", "private-title"] {
        assert!(!text.contains(secret), "{text}");
    }
    for hint in [
        "readOnlyHint",
        "destructiveHint",
        "annotations_are_untrusted_hints",
    ] {
        assert!(text.contains(hint), "{text}");
    }
}

// Catches: credentials inside a serialized JSON request body leave the machine for judging.
#[test]
fn mcp_serialized_json_body_does_not_disclose_password_to_judge() {
    let call = mcp_call(json!({
        "tool_name": "http_request",
        "arguments": {
            "method": "POST",
            "body": r#"{"username":"fixture-user","password":"fixture-private-password"}"#
        }
    }));
    let request = support::judge_request(&call, &JudgeTaskContext::default());
    let text = serde_json::to_string(&request.messages).unwrap();
    assert!(
        !text.contains("fixture-private-password"),
        "serialized JSON credential leaked to judge: {text}"
    );
}

// Catches: oversized or deeply nested arguments are silently presented as complete.
#[test]
fn mcp_evidence_marks_truncation_and_bounds_parameter_disclosure() {
    for parameters in [
        json!({"message":"x ".repeat(20000)}),
        json!({"a":{"b":{"c":{"d":{"e":{"f":{"g":{"h":{"i":"hidden"}}}}}}}}}),
    ] {
        let text = serde_json::to_string(
            &support::judge_request(&mcp_call(parameters), &JudgeTaskContext::default()).messages,
        )
        .unwrap();
        assert!(text.contains("incomplete"), "{text}");
        assert!(text.len() < 24000, "{}", text.len());
    }
}

// Catches: the server-wide option silently becomes the default or trusts another server.
#[test]
fn server_remembering_is_explicit_and_keeps_other_servers_uncovered() {
    use ego_core::config::ManagedRuleEffect;
    use ego_core::config::rule_management::RuleManagement;
    use std::path::Path;
    let call = mcp_call(json!({"arguments":{}}));
    let exact = RuleManagement::suggest_remember(&call, ManagedRuleEffect::Allow, Path::new("/ws"))
        .unwrap();
    assert_eq!(exact.candidates[0].to_string(), "tool(tuic/call_tool)");
    let server = exact.server_scope().unwrap();
    assert_eq!(server.candidates[0].to_string(), "tool(tuic/*)");
    for (server_name, expected) in [("tuic", true), ("other", false)] {
        let op = ego_core::permit::Op::Opaque {
            tool: ForeignToolId::new(server_name, "progress").unwrap(),
            params_digest: InvocationDigest::from_bytes(b"{}"),
            transport: ego_core::permit::ForeignTransportTarget::acp(server_name).unwrap(),
        };
        assert_eq!(
            server.candidates[0].matches(&op, Path::new("/ws")),
            expected
        );
        assert!(!exact.candidates[0].matches(&op, Path::new("/ws")));
    }
}

// Catches: a URL-encoded JSON HTTP body bypasses secret-key redaction.
#[test]
fn mcp_percent_encoded_json_body_does_not_disclose_password_to_judge() {
    let body = "%7B%22username%22%3A%22fixture-user%22%2C%22PASSWORD%22%3A%22fixture-private-password%22%7D";
    let call = mcp_call(json!({"tool_name":"http_request", "arguments":{"body":body}}));
    let request = support::judge_request(&call, &JudgeTaskContext::default());
    let text = serde_json::to_string(&request.messages).unwrap();
    assert!(
        !text.contains("fixture-private-password"),
        "percent-encoded JSON credential leaked to judge: {text}"
    );
}
