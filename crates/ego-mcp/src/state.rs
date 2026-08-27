use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use ego_core::config::McpServerName;
use rmcp::model::RequestStateCodec;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{McpError, McpFailure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheScope {
    Private,
    Shared,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpResultMetadata {
    ttl_ms: Option<u64>,
    cache_scope: CacheScope,
}

impl McpResultMetadata {
    pub fn new(ttl_ms: Option<u64>, cache_scope: CacheScope) -> Result<Self, McpError> {
        if ttl_ms == Some(0) {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "cache TTL must be positive",
            ));
        }
        Ok(Self {
            ttl_ms,
            cache_scope,
        })
    }

    pub fn ttl_ms(&self) -> Option<u64> {
        self.ttl_ms
    }

    pub fn cache_scope(&self) -> CacheScope {
        self.cache_scope
    }
}

pub struct McpToolResult {
    body: Vec<u8>,
    cache: McpResultMetadata,
}

impl McpToolResult {
    pub fn new(body: impl AsRef<[u8]>, cache: McpResultMetadata) -> Result<Self, McpError> {
        let body = body.as_ref().to_vec();
        let value: Value = serde_json::from_slice(&body)?;
        if !value.is_object() {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "tool result must be an object",
            ));
        }
        Ok(Self { body, cache })
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn cache(&self) -> &McpResultMetadata {
        &self.cache
    }
}

pub struct McpSubscription {
    server: McpServerName,
    kind: SubscriptionKind,
}

enum SubscriptionKind {
    Tools,
}

impl McpSubscription {
    pub fn tools(server: McpServerName) -> Result<Self, McpError> {
        Ok(Self {
            server,
            kind: SubscriptionKind::Tools,
        })
    }

    pub fn resource_content(_server: McpServerName) -> Result<Self, McpError> {
        Err(McpError::new(
            McpFailure::Protocol,
            "resource content subscriptions are not in MCP 2026-07-28",
        ))
    }

    pub fn listen_method(&self) -> &'static str {
        "subscriptions/listen"
    }

    pub fn notification_method(&self) -> &'static str {
        match self.kind {
            SubscriptionKind::Tools => "notifications/tools/list_changed",
        }
    }

    pub fn apply(&self, event: McpSubscriptionEvent) -> Result<McpRefreshRequest, McpError> {
        match (&self.kind, event) {
            (SubscriptionKind::Tools, McpSubscriptionEvent::ToolsListChanged { .. }) => {
                Ok(McpRefreshRequest {
                    server: self.server.clone(),
                    method: "tools/list",
                    complete_snapshot: true,
                })
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpSubscriptionEvent {
    ToolsListChanged { generation: u64 },
}

pub struct McpRefreshRequest {
    server: McpServerName,
    method: &'static str,
    complete_snapshot: bool,
}

impl McpRefreshRequest {
    pub fn server(&self) -> &McpServerName {
        &self.server
    }

    pub fn method(&self) -> &'static str {
        self.method
    }

    pub fn is_complete_snapshot(&self) -> bool {
        self.complete_snapshot
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct McpInputRequestId(String);

struct PendingInput {
    schema: Value,
    server: McpServerName,
    method: String,
    call_id: String,
    original: Vec<u8>,
    sealed: String,
}

static NEXT_INPUT_ID: AtomicU64 = AtomicU64::new(1);
static PENDING: OnceLock<Mutex<HashMap<McpInputRequestId, PendingInput>>> = OnceLock::new();
static STATE_CODEC: OnceLock<RequestStateCodec> = OnceLock::new();

fn pending() -> &'static Mutex<HashMap<McpInputRequestId, PendingInput>> {
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

fn state_codec() -> &'static RequestStateCodec {
    STATE_CODEC.get_or_init(|| {
        let mut digest = Sha256::new();
        digest.update(b"ego:mcp:request-state-key:v1");
        digest.update(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_be_bytes(),
        );
        digest.update(format!("{:p}", &PENDING).as_bytes());
        RequestStateCodec::try_new(digest.finalize().to_vec())
            .expect("SHA-256 produces the required key length")
    })
}

pub struct McpInputState {
    request_id: McpInputRequestId,
    sealed_state: String,
}

impl McpInputState {
    pub fn required(
        server: McpServerName,
        method: &str,
        call_id: &str,
        schema: impl AsRef<[u8]>,
        original: impl AsRef<[u8]>,
    ) -> Result<Self, McpError> {
        if method.is_empty() || call_id.is_empty() {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "invalid MRTR identity",
            ));
        }
        let schema: Value = serde_json::from_slice(schema.as_ref())?;
        validate_schema_shape(&schema)?;
        let original = original.as_ref().to_vec();
        let _: Value = serde_json::from_slice(&original)?;
        let serial = NEXT_INPUT_ID.fetch_add(1, Ordering::Relaxed);
        let request_id = McpInputRequestId(format!("mrtr-{serial}"));
        let context = mrtr_context(&server, method, call_id, &original);
        let sealed_state = state_codec().seal_with(
            request_id.0.as_bytes(),
            &rmcp::model::SealOptions::new().associated_data(&context),
        );
        pending()
            .lock()
            .map_err(|_| McpError::new(McpFailure::Protocol, "MRTR state lock poisoned"))?
            .insert(
                request_id.clone(),
                PendingInput {
                    schema,
                    server,
                    method: method.to_owned(),
                    call_id: call_id.to_owned(),
                    original,
                    sealed: sealed_state.clone(),
                },
            );
        Ok(Self {
            request_id,
            sealed_state,
        })
    }

    pub fn method(&self) -> &'static str {
        "input_required"
    }

    pub fn request_id(&self) -> &McpInputRequestId {
        &self.request_id
    }

    pub fn sealed_state(&self) -> &str {
        &self.sealed_state
    }

    pub fn resume(self, response: McpInputResponse) -> Result<McpRetryRequest, McpError> {
        if response.request_id != self.request_id {
            return Err(McpError::new(
                McpFailure::Protocol,
                "MRTR response identity mismatch",
            ));
        }
        let pending = pending()
            .lock()
            .map_err(|_| McpError::new(McpFailure::Protocol, "MRTR state lock poisoned"))?
            .remove(&self.request_id)
            .ok_or_else(|| {
                McpError::new(McpFailure::Protocol, "MRTR state was already consumed")
            })?;
        if pending.sealed != self.sealed_state {
            return Err(McpError::new(
                McpFailure::Protocol,
                "MRTR sealed state mismatch",
            ));
        }
        let context = mrtr_context(
            &pending.server,
            &pending.method,
            &pending.call_id,
            &pending.original,
        );
        let opened = state_codec()
            .open_with(&self.sealed_state, &context)
            .map_err(|error| McpError::new(McpFailure::Protocol, error.to_string()))?;
        if opened != self.request_id.0.as_bytes() {
            return Err(McpError::new(
                McpFailure::Protocol,
                "MRTR state payload mismatch",
            ));
        }
        let mut original: Value = serde_json::from_slice(&pending.original)?;
        let object = original.as_object_mut().ok_or_else(|| {
            McpError::new(
                McpFailure::InvalidValue,
                "MRTR original operation is not an object",
            )
        })?;
        object.insert(
            "inputResponses".to_owned(),
            Value::Array(vec![response.answer]),
        );
        Ok(McpRetryRequest {
            method: pending.method,
            call_id: pending.call_id,
            body: serde_json::to_vec(&original)?,
        })
    }
}

pub struct McpInputResponse {
    request_id: McpInputRequestId,
    answer: Value,
}

impl McpInputResponse {
    pub fn new(request_id: &McpInputRequestId, answer: impl AsRef<[u8]>) -> Result<Self, McpError> {
        let answer: Value = serde_json::from_slice(answer.as_ref())?;
        let guard = pending()
            .lock()
            .map_err(|_| McpError::new(McpFailure::Protocol, "MRTR state lock poisoned"))?;
        let state = guard
            .get(request_id)
            .ok_or_else(|| McpError::new(McpFailure::Protocol, "unknown MRTR request"))?;
        validate_answer(&state.schema, &answer)?;
        Ok(Self {
            request_id: request_id.clone(),
            answer,
        })
    }
}

pub struct McpRetryRequest {
    method: String,
    call_id: String,
    body: Vec<u8>,
}

impl McpRetryRequest {
    pub fn method(&self) -> &str {
        &self.method
    }

    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

fn validate_schema_shape(schema: &Value) -> Result<(), McpError> {
    let object = schema
        .as_object()
        .ok_or_else(|| McpError::new(McpFailure::InvalidValue, "MRTR schema is not an object"))?;
    if object.get("type").and_then(Value::as_str) != Some("object")
        || !object.get("properties").is_some_and(Value::is_object)
    {
        return Err(McpError::new(
            McpFailure::InvalidValue,
            "unsupported MRTR schema",
        ));
    }
    Ok(())
}

fn validate_answer(schema: &Value, answer: &Value) -> Result<(), McpError> {
    let schema = schema.as_object().expect("validated schema");
    let answer = answer
        .as_object()
        .ok_or_else(|| McpError::new(McpFailure::InvalidValue, "MRTR answer is not an object"))?;
    let properties = schema["properties"]
        .as_object()
        .expect("validated properties");
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for name in required.iter().filter_map(Value::as_str) {
        if !answer.contains_key(name) {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "MRTR answer misses a required field",
            ));
        }
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(false))
        && answer.keys().any(|name| !properties.contains_key(name))
    {
        return Err(McpError::new(
            McpFailure::InvalidValue,
            "MRTR answer has an unknown field",
        ));
    }
    for (name, value) in answer {
        let Some(property) = properties.get(name).and_then(Value::as_object) else {
            continue;
        };
        let valid = match property.get("type").and_then(Value::as_str) {
            Some("boolean") => value.is_boolean(),
            Some("string") => value.is_string(),
            Some("number") => value.is_number(),
            Some("object") => value.is_object(),
            Some("array") => value.is_array(),
            _ => false,
        };
        if !valid {
            return Err(McpError::new(
                McpFailure::InvalidValue,
                "MRTR answer has a wrong type",
            ));
        }
    }
    Ok(())
}

fn mrtr_context(server: &McpServerName, method: &str, call_id: &str, original: &[u8]) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(b"ego:mcp:mrtr-context:v1");
    digest.update(server.as_str().as_bytes());
    digest.update(method.as_bytes());
    digest.update(call_id.as_bytes());
    digest.update(original);
    digest.finalize().to_vec()
}
