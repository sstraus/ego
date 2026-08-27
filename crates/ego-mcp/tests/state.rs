//! RED contract for MCP result metadata, tool subscriptions, and MRTR state.

use ego_mcp::{
    CacheScope, McpInputResponse, McpInputState, McpResultMetadata, McpSubscription,
    McpSubscriptionEvent, McpToolResult,
};

#[test]
fn result_metadata_preserves_ttl_and_cache_scope_without_guessing_scope() {
    let private = McpResultMetadata::new(Some(30_000), CacheScope::Private).expect("metadata");
    assert_eq!(private.ttl_ms(), Some(30_000));
    assert_eq!(private.cache_scope(), CacheScope::Private);

    let shared = McpResultMetadata::new(Some(1_000), CacheScope::Shared).expect("metadata");
    assert_eq!(shared.ttl_ms(), Some(1_000));
    assert_eq!(shared.cache_scope(), CacheScope::Shared);

    assert!(McpResultMetadata::new(Some(0), CacheScope::Private).is_err());
    let result = McpToolResult::new(br#"{"content":[{"type":"text","text":"safe"}]}"#, private)
        .expect("tool result");
    assert_eq!(result.cache().ttl_ms(), Some(30_000));
    assert_eq!(result.cache().cache_scope(), CacheScope::Private);
}

#[test]
fn tool_list_subscription_requires_current_listen_and_refreshes_whole_snapshot() {
    let subscription =
        McpSubscription::tools("remote".parse().expect("server name")).expect("tool subscription");
    assert_eq!(subscription.listen_method(), "subscriptions/listen");
    assert_eq!(
        subscription.notification_method(),
        "notifications/tools/list_changed"
    );

    let update = McpSubscriptionEvent::ToolsListChanged { generation: 8 };
    let refresh = subscription.apply(update).expect("refresh request");
    assert_eq!(refresh.method(), "tools/list");
    assert!(refresh.is_complete_snapshot());
    assert!(McpSubscription::resource_content("remote".parse().expect("server name")).is_err());
}

#[test]
fn mrtr_seals_request_state_and_retries_only_original_operation_with_validated_answers() {
    let pending = McpInputState::required(
        "remote".parse().expect("server name"),
        "tools/call",
        "call-7",
        br#"{"type":"object","properties":{"confirmed":{"type":"boolean"}},"required":["confirmed"],"additionalProperties":false}"#,
        br#"{"name":"confirm_delete","arguments":{"path":"/tmp/x"}}"#,
    )
    .expect("MRTR request");
    assert_eq!(pending.method(), "input_required");
    assert!(!pending.sealed_state().is_empty());
    let request_id = pending.request_id().clone();

    let response =
        McpInputResponse::new(&request_id, br#"{"confirmed":true}"#).expect("validated response");
    let retry = pending.resume(response).expect("original operation retry");
    assert_eq!(retry.method(), "tools/call");
    assert_eq!(retry.call_id(), "call-7");
    assert!(
        retry
            .body()
            .windows(b"inputResponses".len())
            .any(|window| { window == b"inputResponses" })
    );
    assert!(McpInputResponse::new(&request_id, br#"{"unexpected":true}"#).is_err());
    assert!(
        !retry
            .body()
            .windows(b"sealed_state".len())
            .any(|window| { window == b"sealed_state" })
    );
}
