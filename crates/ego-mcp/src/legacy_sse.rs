//! Explicit HTTP+SSE framing over the same admitted HTTP effects as current MCP.
//! RMCP, not this adapter, owns initialize, request IDs, and JSON-RPC routing.

use super::*;
use rmcp::transport::Transport;
use url::Url;

pub(super) const REVISION: &str = "2024-11-05";

// An SSE POST can be abandoned while the receiving task observes cancellation.
// Unlike a current request, it cannot rely on the stream's guard to settle it.
struct LegacyGuard(Option<ActiveHttpGuard>);

impl LegacyGuard {
    async fn cancel(mut self) -> Result<(), McpError> {
        self.0
            .take()
            .expect("owned legacy HTTP guard")
            .cancel()
            .await
    }
    async fn timeout(mut self) -> Result<(), McpError> {
        self.0
            .take()
            .expect("owned legacy HTTP guard")
            .timeout()
            .await
    }
    async fn settle(mut self) -> Result<(), McpError> {
        self.0
            .take()
            .expect("owned legacy HTTP guard")
            .settle()
            .await
    }
}

impl Drop for LegacyGuard {
    fn drop(&mut self) {
        if let Some(guard) = self.0.take() {
            let _ = futures::executor::block_on(guard.cancel());
        }
    }
}

#[derive(Clone)]
struct Sender {
    server: McpServerName,
    url: Url,
    profile: AdmittedMcpHttpProfile,
    effects: Arc<dyn McpHttpEffects>,
    options: McpClientOptions,
    control: Arc<ClientControl>,
}

impl Sender {
    async fn execute(
        &self,
        method: &str,
        body: Vec<u8>,
    ) -> Result<(crate::McpHttpResponse, LegacyGuard), McpError> {
        let headers = vec![
            header(
                "accept",
                if method == "GET" {
                    b"text/event-stream".to_vec()
                } else {
                    b"application/json".to_vec()
                },
            )?,
            header("content-type", b"application/json".to_vec())?,
        ];
        let request = HttpRequest::new(method, &path_and_query(&self.url), headers, body)
            .map_err(|_| protocol("invalid legacy SSE HTTP request"))?;
        let call = self.profile.prepare(
            &self.server,
            McpHttpPurpose::Protocol,
            &self.url,
            request,
            self.options.deadline,
        )?;
        // A legacy stream and its POST may coexist. Each still has its own
        // effect-owned identity and settlement; neither borrows the other's guard.
        let guard = LegacyGuard(Some(ActiveHttpGuard {
            request_id: call.request_id().clone(),
            effects: Arc::clone(&self.effects),
            control: Arc::clone(&self.control),
            finished: false,
        }));
        let outcome = tokio::select! {
            biased;
            () = wait_cancelled(Arc::clone(&self.control)) => {
                guard.cancel().await?;
                return Err(McpError::new(McpFailure::Cancelled, "legacy SSE operation cancelled"));
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(self.options.deadline)) => {
                guard.timeout().await?;
                return Err(protocol("legacy SSE deadline elapsed"));
            }
            response = self.effects.execute(call) => response?,
        };
        if outcome.location().is_some() {
            guard.cancel().await?;
            return Err(protocol("legacy SSE redirects are not supported"));
        }
        Ok((outcome, guard))
    }
}

pub(super) struct LegacySse {
    sender: Sender,
    stream: BoxStream<'static, Result<sse_stream::Sse, std::io::Error>>,
    guard: Option<LegacyGuard>,
}

impl LegacySse {
    pub(super) async fn open(
        server: McpServerName,
        config: McpHttpConfig,
        profile: AdmittedMcpHttpProfile,
        effects: Arc<dyn McpHttpEffects>,
        options: McpClientOptions,
        control: Arc<ClientControl>,
    ) -> Result<Self, McpError> {
        let mut sender = Sender {
            server,
            url: config.url.as_url().clone(),
            profile,
            effects,
            options,
            control,
        };
        let (response, guard) = sender.execute("GET", Vec::new()).await?;
        if response.status() != 200
            || !response.header_value("content-type").is_some_and(|value| {
                value.split(|byte| *byte == b';').next() == Some(b"text/event-stream".as_slice())
            })
        {
            guard.cancel().await?;
            return Err(protocol(
                "legacy SSE GET requires a text/event-stream response",
            ));
        }
        let body = response
            .into_body()
            .ok_or_else(|| protocol("legacy SSE stream has no body"))?;
        let bytes = futures::stream::unfold(
            (
                body,
                SseEventSizeLimiter::new(options.max_sse_event_size),
                false,
            ),
            |(mut body, mut limiter, failed)| async move {
                if failed {
                    return None;
                }
                let item = body.next().await?;
                let item = item.map_err(std::io::Error::other).and_then(|chunk| {
                    limiter.observe(&chunk)?;
                    Ok(Cursor::new(chunk))
                });
                let failed = item.is_err();
                Some((item, (body, limiter, failed)))
            },
        );
        let stream = SseStream::from_bytes_stream(bytes)
            .map(|event| event.map_err(std::io::Error::other))
            .boxed();
        let mut transport = Self {
            sender: sender.clone(),
            stream,
            guard: Some(guard),
        };
        let event = transport
            .event()
            .await?
            .ok_or_else(|| protocol("legacy SSE endpoint missing"))?;
        if event.event.as_deref() != Some("endpoint") {
            return Err(protocol("legacy SSE first event must select an endpoint"));
        }
        let endpoint = event
            .data
            .ok_or_else(|| protocol("legacy SSE endpoint missing"))?;
        let url = sender
            .url
            .join(&endpoint)
            .map_err(|_| protocol("invalid legacy SSE endpoint"))?;
        // An endpoint event is untrusted routing data, not new authority. Do not
        // admit an additional origin, follow a redirect, or forward credentials.
        if url.origin() != sender.url.origin()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
            || endpoint.trim() != endpoint
            || endpoint
                .strip_prefix("//")
                .or_else(|| endpoint.split_once("://").map(|(_, rest)| rest))
                .is_some_and(|rest| {
                    rest.split(['/', '?', '#'])
                        .next()
                        .is_some_and(|a| a.contains('@'))
                })
        {
            return Err(McpError::new(
                McpFailure::Authority,
                "legacy SSE endpoint exceeds its admitted origin",
            ));
        }
        sender.profile.route(&url)?;
        sender.url = url;
        transport.sender = sender;
        Ok(transport)
    }

    async fn event(&mut self) -> Result<Option<sse_stream::Sse>, McpError> {
        let outcome = tokio::select! {
            biased;
            () = wait_cancelled(Arc::clone(&self.sender.control)) => {
                if let Some(guard) = self.guard.take() { guard.cancel().await?; }
                return Err(McpError::new(McpFailure::Cancelled, "legacy SSE stream cancelled"));
            }
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(self.sender.options.deadline)) => {
                if let Some(guard) = self.guard.take() { guard.timeout().await?; }
                return Err(protocol("legacy SSE stream deadline elapsed"));
            }
            event = self.stream.next() => event,
        };
        match outcome {
            Some(Ok(event)) => Ok(Some(event)),
            Some(Err(_)) => {
                if let Some(guard) = self.guard.take() {
                    guard.cancel().await?;
                }
                Err(protocol("invalid or oversized legacy SSE event"))
            }
            None => {
                if let Some(guard) = self.guard.take() {
                    guard.settle().await?;
                }
                Ok(None)
            }
        }
    }
}

impl Transport<RoleClient> for LegacySse {
    type Error = McpError;

    fn send(
        &mut self,
        message: ClientJsonRpcMessage,
    ) -> impl std::future::Future<Output = Result<(), McpError>> + Send + 'static {
        let sender = self.sender.clone();
        async move {
            let body =
                serde_json::to_vec(&message).map_err(|_| protocol("invalid legacy SSE message"))?;
            if body.len() > sender.options.max_sse_event_size {
                return Err(protocol("legacy SSE outgoing frame exceeds its bound"));
            }
            let (response, guard) = sender.execute("POST", body).await?;
            if response.status() != 202 {
                guard.cancel().await?;
                return Err(protocol("legacy SSE POST was not accepted"));
            }
            // Even an accepted POST may carry a streaming body. Drain only a
            // bounded response and retain cancellation/deadline ownership.
            let limit = sender.options.max_sse_event_size;
            tokio::select! {
                biased;
                () = wait_cancelled(Arc::clone(&sender.control)) => {
                    guard.cancel().await?;
                    return Err(McpError::new(McpFailure::Cancelled, "legacy SSE POST cancelled"));
                }
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(sender.options.deadline)) => {
                    guard.timeout().await?;
                    return Err(protocol("legacy SSE POST deadline elapsed"));
                }
                result = response.into_bytes(limit) => { result?; }
            }
            guard.settle().await
        }
    }

    async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
        let event = self.event().await.ok()??;
        if event.event.as_deref() != Some("message") {
            return None;
        }
        let mut frame: serde_json::Value = serde_json::from_str(event.data.as_deref()?).ok()?;
        if let Some(result) = frame
            .get_mut("result")
            .and_then(serde_json::Value::as_object_mut)
        {
            if let Some(version) = result.get("protocolVersion") {
                if version.as_str() != Some(REVISION) {
                    return None;
                }
            } else {
                // A legacy response is complete, with no reusable cache grant.
                // This projection is confined to the explicitly selected adapter.
                if result
                    .get("resultType")
                    .is_some_and(|value| value.as_str() != Some("complete"))
                {
                    return None;
                }
                result.insert("resultType".into(), serde_json::json!("complete"));
                if [
                    "tools",
                    "resources",
                    "resourceTemplates",
                    "prompts",
                    "contents",
                ]
                .iter()
                .any(|key| result.contains_key(*key))
                {
                    result.insert("ttlMs".into(), serde_json::json!(0));
                    result.insert("cacheScope".into(), serde_json::json!("private"));
                }
            }
        }
        serde_json::from_value(frame).ok()
    }

    async fn close(&mut self) -> Result<(), McpError> {
        if let Some(guard) = self.guard.take() {
            guard.cancel().await?;
        }
        Ok(())
    }
}

fn protocol(message: &'static str) -> McpError {
    McpError::new(McpFailure::Protocol, message)
}
