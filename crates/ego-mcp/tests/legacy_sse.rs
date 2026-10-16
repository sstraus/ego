//! Replay real exchanges from the official SDK, not a hand-written MCP peer.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ego_core::config::{McpHttpAuth, McpHttpConfig, McpHttpProfile};
use ego_core::generator::attempt::DeliveryCertainty;
use ego_core::permit::HttpHeader;
use ego_mcp::{
    McpClient, McpClientOptions, McpError, McpFuture, McpHttpEffects, McpHttpEnd, McpHttpResponse,
    McpHttpSettlement, McpManualOutcome, McpRequestId, PreparedMcpHttpCall,
};
use futures::{StreamExt, channel::mpsc};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/legacy-sse-sdk-1.32.1.json")).unwrap()
}

type Seen = (String, String, Value);
struct Replay {
    fixture: Value,
    seen: Mutex<Vec<Seen>>,
    events: Mutex<Option<mpsc::UnboundedSender<Result<Vec<u8>, McpError>>>>,
    settlements: Mutex<Vec<McpHttpEnd>>,
    endpoint: Option<String>,
    stall_endpoint: bool,
    stall_get: bool,
    redirect: bool,
    post_status: u16,
}

impl Replay {
    fn new() -> Self {
        Self {
            fixture: fixture(),
            seen: Mutex::new(Vec::new()),
            events: Mutex::new(None),
            settlements: Mutex::new(Vec::new()),
            endpoint: None,
            stall_endpoint: false,
            stall_get: false,
            redirect: false,
            post_status: 202,
        }
    }

    fn client(self: &Arc<Self>, profile: McpHttpProfile, bound: usize) -> McpClient {
        let mut config = McpHttpConfig::new(
            "http://127.0.0.1:8787/mcp".parse::<url::Url>().unwrap(),
            McpHttpAuth::None,
            true,
        )
        .unwrap();
        config.profile = profile;
        McpClient::connect_http(
            "sdk".parse().unwrap(),
            config,
            McpClientOptions::new(bound, Instant::now() + Duration::from_secs(2)).unwrap(),
            Arc::clone(self) as Arc<dyn McpHttpEffects>,
        )
        .unwrap()
    }

    fn finish(
        &self,
        id: McpRequestId,
        end: McpHttpEnd,
    ) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        self.settlements.lock().unwrap().push(end);
        Box::pin(async move { McpHttpSettlement::new(id, end, DeliveryCertainty::PossiblySent) })
    }
}

impl McpHttpEffects for Replay {
    fn execute(
        &self,
        call: PreparedMcpHttpCall,
    ) -> McpFuture<'static, Result<McpHttpResponse, McpError>> {
        let request = call.request();
        let frame: Value = serde_json::from_slice(request.body()).unwrap_or(Value::Null);
        self.seen.lock().unwrap().push((
            request.method().to_owned(),
            request.path().to_owned(),
            frame.clone(),
        ));
        if request.method() == "GET" {
            if self.stall_get {
                return Box::pin(futures::future::pending());
            }
            if self.redirect {
                return Box::pin(async {
                    Ok(McpHttpResponse::redirect(
                        302,
                        Vec::new(),
                        b"https://other.test".to_vec(),
                    ))
                });
            }
            let (tx, rx) = mpsc::unbounded();
            if !self.stall_endpoint {
                let event = self.endpoint.as_ref().map_or_else(
                    || self.fixture["frames"][0].as_str().unwrap().to_owned(),
                    |endpoint| format!("event: endpoint\ndata: {endpoint}\n\n"),
                );
                tx.unbounded_send(Ok(event.into_bytes())).unwrap();
            }
            *self.events.lock().unwrap() = Some(tx);
            return Box::pin(async move {
                McpHttpResponse::response(
                    200,
                    vec![HttpHeader::new("content-type", b"text/event-stream".to_vec()).unwrap()],
                    rx.boxed(),
                    DeliveryCertainty::PossiblySent,
                )
            });
        }
        let method = frame["method"].as_str().unwrap_or("");
        let index = match method {
            "initialize" => Some(1),
            "tools/list" => Some(2),
            "tools/call" => Some(4),
            _ => None,
        };
        // A current POST to this SSE-only endpoint is refused, not converted to GET.
        let status = if method == "server/discover" {
            405
        } else {
            self.post_status
        };
        if status == 202
            && let Some(index) = index
        {
            let raw = self.fixture["frames"][index].as_str().unwrap();
            let data = raw.split("data: ").nth(1).unwrap().trim();
            let mut reply: Value = serde_json::from_str(data).unwrap();
            reply["id"] = frame["id"].clone();
            if let Some(tx) = self.events.lock().unwrap().as_ref() {
                tx.unbounded_send(Ok(format!("event: message\ndata: {reply}\n\n").into_bytes()))
                    .unwrap();
            }
        }
        Box::pin(async move { Ok(McpHttpResponse::json(status, b"Accepted")) })
    }
    fn cancel(&self, id: McpRequestId) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        self.finish(id, McpHttpEnd::Cancelled)
    }
    fn timeout(&self, id: McpRequestId) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        self.finish(id, McpHttpEnd::TimedOut)
    }
    fn settle(&self, id: McpRequestId) -> McpFuture<'static, Result<McpHttpSettlement, McpError>> {
        self.finish(id, McpHttpEnd::Completed)
    }
}

#[tokio::test]
async fn explicit_sse_replays_sdk_inventory_instead_of_sending_current_discover() {
    let effects = Arc::new(Replay::new());
    let mut client = effects.client(McpHttpProfile::LegacySse, 4096);
    client.discover().await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools[0].name, "start-notification-stream");
    assert_eq!(
        tools[0].description,
        "Starts sending periodic notifications"
    );
    client.settle().await.unwrap();
    let seen = effects.seen.lock().unwrap();
    assert_eq!(seen[0].0, "GET");
    assert_eq!(seen[0].1, "/mcp");
    assert_eq!(seen[1].1, fixture()["endpoint"].as_str().unwrap());
    assert_eq!(seen[1].2["method"], "initialize");
    assert_eq!(seen[1].2["params"]["protocolVersion"], "2024-11-05");
    assert_eq!(seen[2].2["method"], "notifications/initialized");
    assert_eq!(seen[3].2["method"], "tools/list");
    assert!(
        effects
            .settlements
            .lock()
            .unwrap()
            .contains(&McpHttpEnd::Cancelled)
    );
}

#[tokio::test]
async fn legacy_tool_response_is_not_rejected_for_missing_current_result_type() {
    let effects = Arc::new(Replay::new());
    let mut client = effects.client(McpHttpProfile::LegacySse, 4096);
    client.discover().await.unwrap();
    let params = rmcp::model::CallToolRequestParams::new("start-notification-stream")
        .with_arguments(json!({"count":0,"interval":1}).as_object().unwrap().clone());
    let outcome = client.call_tool_once(params).await.unwrap();
    let McpManualOutcome::Complete(result) = outcome else {
        panic!("legacy server cannot request MRTR");
    };
    let value = serde_json::to_value(result).unwrap();
    assert_eq!(
        value["content"][0]["text"],
        "Completed sending 0 notifications every 1ms"
    );
    client.settle().await.unwrap();
}

#[tokio::test]
async fn absent_sse_profile_refuses_sse_only_server_without_a_get_downgrade() {
    let effects = Arc::new(Replay::new());
    let mut client = effects.client(McpHttpProfile::Current, 4096);
    assert!(client.discover().await.is_err());
    client.settle().await.unwrap();
    let seen = effects.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "POST");
    assert_eq!(seen[0].2["method"], "server/discover");
}

#[tokio::test]
async fn sse_endpoint_cannot_forward_authority_to_another_origin_or_userinfo() {
    for endpoint in [
        "https://other.test/messages",
        "http://127.0.0.1:9999/messages",
        "http://u:p@127.0.0.1:8787/messages",
        "/messages#fragment",
        "//@127.0.0.1:8787/messages",
    ] {
        let mut replay = Replay::new();
        replay.endpoint = Some(endpoint.to_owned());
        let effects = Arc::new(replay);
        let mut client = effects.client(McpHttpProfile::LegacySse, 4096);
        assert!(client.discover().await.is_err(), "{endpoint}");
        client.settle().await.unwrap();
        assert_eq!(
            effects.seen.lock().unwrap().len(),
            1,
            "no POST authority: {endpoint}"
        );
        assert_eq!(
            *effects.settlements.lock().unwrap(),
            [McpHttpEnd::Cancelled]
        );
    }
}

#[tokio::test]
async fn oversized_sse_endpoint_and_redirect_are_refused_before_post() {
    for redirect in [false, true] {
        let mut replay = Replay::new();
        replay.redirect = redirect;
        let effects = Arc::new(replay);
        let mut client = effects.client(McpHttpProfile::LegacySse, 16);
        assert!(client.discover().await.is_err());
        client.settle().await.unwrap();
        assert_eq!(effects.seen.lock().unwrap().len(), 1);
        assert_eq!(
            *effects.settlements.lock().unwrap(),
            [McpHttpEnd::Cancelled]
        );
    }
}

#[tokio::test]
async fn cancellation_owns_get_before_headers_and_before_endpoint() {
    for before_headers in [true, false] {
        let mut replay = Replay::new();
        replay.stall_get = before_headers;
        replay.stall_endpoint = !before_headers;
        let effects = Arc::new(replay);
        let mut client = effects.client(McpHttpProfile::LegacySse, 4096);
        let cancel = client.cancellation();
        let cancel_after_yield = async move {
            tokio::task::yield_now().await;
            cancel.cancel();
        };
        let (outcome, ()) = tokio::join!(client.discover(), cancel_after_yield);
        assert!(outcome.is_err());
        client.settle().await.unwrap();
        assert_eq!(
            *effects.settlements.lock().unwrap(),
            [McpHttpEnd::Cancelled]
        );
    }
}

#[tokio::test]
async fn refused_legacy_post_does_not_leave_its_receiving_stream_open() {
    let mut replay = Replay::new();
    replay.post_status = 403;
    let effects = Arc::new(replay);
    let mut client = effects.client(McpHttpProfile::LegacySse, 4096);
    assert!(client.discover().await.is_err());
    client.settle().await.unwrap();
    assert_eq!(
        *effects.settlements.lock().unwrap(),
        [McpHttpEnd::Cancelled, McpHttpEnd::Cancelled]
    );
}

#[tokio::test]
async fn legacy_initialize_cannot_negotiate_an_unselected_revision() {
    let mut replay = Replay::new();
    let init = replay.fixture["frames"][1]
        .as_str()
        .unwrap()
        .replace("2024-11-05", "2025-11-25");
    replay.fixture["frames"][1] = json!(init);
    let effects = Arc::new(replay);
    let mut client = effects.client(McpHttpProfile::LegacySse, 4096);
    assert!(client.discover().await.is_err());
    client.settle().await.unwrap();
    assert_eq!(
        effects.seen.lock().unwrap().len(),
        2,
        "no initialized notification for a different revision"
    );
}

#[tokio::test]
async fn oversized_legacy_tool_arguments_are_refused_before_post() {
    let effects = Arc::new(Replay::new());
    let mut client = effects.client(McpHttpProfile::LegacySse, 4096);
    client.discover().await.unwrap();
    let params = rmcp::model::CallToolRequestParams::new("start-notification-stream")
        .with_arguments(
            json!({"payload": "x".repeat(8192)})
                .as_object()
                .unwrap()
                .clone(),
        );
    assert!(client.call_tool_once(params).await.is_err());
    client.settle().await.unwrap();
    assert_eq!(
        effects.seen.lock().unwrap().len(),
        3,
        "oversized tool frame crossed the effect boundary"
    );
}
