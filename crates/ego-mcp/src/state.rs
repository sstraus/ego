//! Current result metadata, subscriptions, and manual MRTR projection.

use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ego_core::config::McpServerName;
use ego_core::question::{QuestionMode, QuestionRequest};
use jsonschema::Validator;
use rmcp::model::{
    CallToolResponse, CallToolResult, ElicitRequestParams, GetPromptResponse, GetPromptResult,
    InputRequest, InputRequiredResult, ReadResourceResponse, ReadResourceResult,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::{McpError, McpFailure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheScope {
    Public,
    Private,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpResultMetadata {
    ttl_ms: Option<u64>,
    cache_scope: CacheScope,
}

impl McpResultMetadata {
    pub fn new(ttl_ms: Option<u64>, cache_scope: CacheScope) -> Result<Self, McpError> {
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

    pub fn is_immediately_stale(&self) -> bool {
        self.ttl_ms == Some(0)
    }

    /// Project cache fields according to the selected current method. Every
    /// selected result requires `resultType`; cache fields are mandatory only
    /// where the current protocol defines them.
    pub fn from_result(method: &str, result: &Value) -> Result<Option<Self>, McpError> {
        let object = result
            .as_object()
            .ok_or_else(|| protocol("current MCP result is not an object"))?;
        if object.get("resultType").and_then(Value::as_str) != Some("complete") {
            return Err(protocol("current MCP result requires resultType complete"));
        }
        match method {
            "tools/list"
            | "resources/list"
            | "resources/templates/list"
            | "prompts/list"
            | "resources/read" => {
                let ttl_ms = object
                    .get("ttlMs")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| protocol("current MCP result requires ttlMs"))?;
                let cache_scope = match object.get("cacheScope").and_then(Value::as_str) {
                    Some("public") => CacheScope::Public,
                    Some("private") => CacheScope::Private,
                    _ => {
                        return Err(protocol(
                            "current MCP result requires public or private cacheScope",
                        ));
                    }
                };
                Ok(Some(Self {
                    ttl_ms: Some(ttl_ms),
                    cache_scope,
                }))
            }
            "tools/call" | "prompts/get" => {
                if object.contains_key("ttlMs") || object.contains_key("cacheScope") {
                    return Err(protocol(
                        "current MCP method does not define cache metadata",
                    ));
                }
                Ok(None)
            }
            _ => Err(protocol("unsupported current MCP result method")),
        }
    }
}

pub struct McpToolResult {
    body: Vec<u8>,
}

impl McpToolResult {
    pub fn new(body: impl AsRef<[u8]>) -> Result<Self, McpError> {
        let body = body.as_ref().to_vec();
        let value: Value = serde_json::from_slice(&body)?;
        McpResultMetadata::from_result("tools/call", &value)?;
        Ok(Self { body })
    }

    pub fn body(&self) -> &[u8] {
        &self.body
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
        Err(protocol(
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InputKind {
    Elicitation,
    Sampling,
    Roots,
    Multiple,
    RequestStateOnly,
}

/// Wire evidence received from one manual `*_once` MRTR round.
pub struct McpInputRequired {
    kind: InputKind,
    schema: Option<Value>,
    correlation_key: Option<String>,
    request_state: Option<Vec<u8>>,
}

impl McpInputRequired {
    pub fn elicitation(
        schema: impl AsRef<[u8]>,
        correlation_key: &str,
        request_state: impl AsRef<[u8]>,
    ) -> Result<Self, McpError> {
        Self::with_schema(
            InputKind::Elicitation,
            schema,
            correlation_key,
            Some(request_state.as_ref().to_vec()),
        )
    }

    pub fn sampling(schema: impl AsRef<[u8]>, correlation_key: &str) -> McpInputRequired {
        Self::unsupported_with_schema(InputKind::Sampling, schema, correlation_key)
    }

    pub fn roots(schema: impl AsRef<[u8]>, correlation_key: &str) -> McpInputRequired {
        Self::unsupported_with_schema(InputKind::Roots, schema, correlation_key)
    }

    pub fn multiple(schema: impl AsRef<[u8]>, correlation_key: &str) -> McpInputRequired {
        Self::unsupported_with_schema(InputKind::Multiple, schema, correlation_key)
    }

    pub fn request_state_only(request_state: impl AsRef<[u8]>) -> McpInputRequired {
        Self {
            kind: InputKind::RequestStateOnly,
            schema: None,
            correlation_key: None,
            request_state: Some(request_state.as_ref().to_vec()),
        }
    }

    fn with_schema(
        kind: InputKind,
        schema: impl AsRef<[u8]>,
        correlation_key: &str,
        request_state: Option<Vec<u8>>,
    ) -> Result<Self, McpError> {
        if correlation_key.is_empty()
            || request_state
                .as_ref()
                .is_some_and(|request_state| request_state.is_empty())
        {
            return Err(invalid("invalid MRTR correlation identity"));
        }
        let schema: Value = serde_json::from_slice(schema.as_ref())?;
        compile_schema(&schema)?;
        Ok(Self {
            kind,
            schema: Some(schema),
            correlation_key: Some(correlation_key.to_owned()),
            request_state,
        })
    }

    fn unsupported_with_schema(
        kind: InputKind,
        schema: impl AsRef<[u8]>,
        correlation_key: &str,
    ) -> Self {
        Self {
            kind,
            schema: serde_json::from_slice(schema.as_ref()).ok(),
            correlation_key: Some(correlation_key.to_owned()),
            request_state: None,
        }
    }
}

/// The sole accepted current input-required shape: one form and its opaque
/// request state.
pub struct McpMappedInput {
    schema: Value,
    validator: Validator,
    correlation_key: String,
    request_state: Vec<u8>,
    question_request: QuestionRequest,
}

impl McpMappedInput {
    pub fn correlation_key(&self) -> &str {
        &self.correlation_key
    }

    pub fn request_state(&self) -> &[u8] {
        &self.request_state
    }

    pub fn schema(&self) -> &Value {
        &self.schema
    }

    pub fn question_request(&self) -> &QuestionRequest {
        &self.question_request
    }
}

pub struct McpInputMapping;

impl McpInputMapping {
    pub fn map(required: McpInputRequired) -> Result<McpMappedInput, McpError> {
        if required.kind != InputKind::Elicitation {
            return Err(protocol("unsupported MCP input-required request"));
        }
        let schema = required
            .schema
            .ok_or_else(|| protocol("elicitation form has no schema"))?;
        let validator = compile_schema(&schema)?;
        let correlation_key = required
            .correlation_key
            .filter(|key| !key.is_empty())
            .ok_or_else(|| protocol("elicitation form has no correlation key"))?;
        let request_state = required
            .request_state
            .filter(|state| !state.is_empty())
            .ok_or_else(|| protocol("elicitation form has no requestState"))?;
        let question_request = QuestionRequest::new(
            "MCP server requires structured input",
            QuestionMode::Form {
                schema: schema.clone(),
            },
            Vec::new(),
            false,
        );
        question_request
            .validate()
            .map_err(|error| protocol(error.to_string()))?;
        Ok(McpMappedInput {
            question_request,
            schema,
            validator,
            correlation_key,
            request_state,
        })
    }

    pub fn from_rmcp(required: InputRequiredResult) -> Result<McpMappedInput, McpError> {
        let request_state = required
            .request_state
            .filter(|state| !state.is_empty())
            .ok_or_else(|| protocol("input-required round has no requestState"))?;
        let requests = required
            .input_requests
            .ok_or_else(|| protocol("requestState-only MRTR rounds are unsupported"))?;
        if requests.len() != 1 {
            return Err(protocol(
                "MRTR round must contain exactly one input request",
            ));
        }
        let (correlation_key, request) = requests
            .into_iter()
            .next()
            .expect("one request was checked");
        let InputRequest::Elicitation(request) = request else {
            return Err(protocol("sampling and roots MRTR requests are unsupported"));
        };
        let ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } = request.params
        else {
            return Err(protocol("URL elicitation is unsupported"));
        };
        let schema = serde_json::to_value(requested_schema)?;
        let mut mapped = Self::map(McpInputRequired {
            kind: InputKind::Elicitation,
            schema: Some(schema.clone()),
            correlation_key: Some(correlation_key),
            request_state: Some(request_state.into_bytes()),
        })?;
        mapped.question_request =
            QuestionRequest::new(message, QuestionMode::Form { schema }, Vec::new(), false);
        mapped
            .question_request
            .validate()
            .map_err(|error| protocol(error.to_string()))?;
        Ok(mapped)
    }
}

/// A manual current-protocol operation never drives an automatic elicitor.
pub enum McpManualOutcome<T> {
    Complete(T),
    InputRequired(Box<McpMappedInput>),
}

impl McpManualOutcome<CallToolResult> {
    pub fn from_call(response: CallToolResponse) -> Result<Self, McpError> {
        match response {
            CallToolResponse::Complete(result) => {
                require_complete(result.result_type.as_ref(), "tools/call")?;
                Ok(Self::Complete(result))
            }
            CallToolResponse::InputRequired(required) => Ok(Self::InputRequired(Box::new(
                McpInputMapping::from_rmcp(required)?,
            ))),
            CallToolResponse::Task(_) => Err(protocol("MCP Tasks are unsupported")),
            _ => Err(protocol("unknown tools/call result type")),
        }
    }
}

impl McpManualOutcome<ReadResourceResult> {
    pub fn from_resource(response: ReadResourceResponse) -> Result<Self, McpError> {
        match response {
            ReadResourceResponse::Complete(result) => {
                require_complete(result.result_type.as_ref(), "resources/read")?;
                if result.ttl_ms.is_none() || result.cache_scope.is_none() {
                    return Err(protocol(
                        "current resources/read result requires ttlMs and cacheScope",
                    ));
                }
                Ok(Self::Complete(result))
            }
            ReadResourceResponse::InputRequired(required) => Ok(Self::InputRequired(Box::new(
                McpInputMapping::from_rmcp(required)?,
            ))),
            _ => Err(protocol("unknown resources/read result type")),
        }
    }
}

impl McpManualOutcome<GetPromptResult> {
    pub fn from_prompt(response: GetPromptResponse) -> Result<Self, McpError> {
        match response {
            GetPromptResponse::Complete(result) => {
                require_complete(result.result_type.as_ref(), "prompts/get")?;
                Ok(Self::Complete(result))
            }
            GetPromptResponse::InputRequired(required) => Ok(Self::InputRequired(Box::new(
                McpInputMapping::from_rmcp(required)?,
            ))),
            _ => Err(protocol("unknown prompts/get result type")),
        }
    }
}

struct InputIdentity {
    value: String,
    validator: Validator,
    claimed: AtomicBool,
}

#[derive(Clone)]
pub struct McpInputRequestId(Arc<InputIdentity>);

impl std::fmt::Debug for McpInputRequestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0.value)
    }
}

impl PartialEq for McpInputRequestId {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for McpInputRequestId {}

impl Hash for McpInputRequestId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

/// One self-contained continuation input. Persistence owns this value; there
/// is no process-global continuation registry.
pub struct McpInputState {
    request_id: McpInputRequestId,
    server: McpServerName,
    method: String,
    call_id: String,
    original: Vec<u8>,
    request_state: Vec<u8>,
}

impl McpInputState {
    pub fn required(
        server: McpServerName,
        method: &str,
        call_id: &str,
        schema: impl AsRef<[u8]>,
        original: impl AsRef<[u8]>,
    ) -> Result<Self, McpError> {
        let schema: Value = serde_json::from_slice(schema.as_ref())?;
        let validator = compile_schema(&schema)?;
        let original = original.as_ref().to_vec();
        validate_original(method, call_id, &original)?;
        let request_state = derive_local_id(&server, method, call_id, &schema, &original);
        Self::from_parts(
            server,
            method,
            call_id.to_owned(),
            validator,
            original,
            request_state.clone().into_bytes(),
            request_state,
        )
    }

    pub fn from_mapping(
        server: McpServerName,
        method: &str,
        original: impl AsRef<[u8]>,
        mapped: McpMappedInput,
    ) -> Result<Self, McpError> {
        let original = original.as_ref().to_vec();
        validate_original(method, &mapped.correlation_key, &original)?;
        let id = derive_local_id(
            &server,
            method,
            &mapped.correlation_key,
            &mapped.schema,
            &original,
        );
        Self::from_parts(
            server,
            method,
            mapped.correlation_key,
            mapped.validator,
            original,
            mapped.request_state,
            id,
        )
    }

    fn from_parts(
        server: McpServerName,
        method: &str,
        call_id: String,
        validator: Validator,
        original: Vec<u8>,
        request_state: Vec<u8>,
        id: String,
    ) -> Result<Self, McpError> {
        if request_state.is_empty() {
            return Err(protocol("MRTR requestState is empty"));
        }
        Ok(Self {
            request_id: McpInputRequestId(Arc::new(InputIdentity {
                value: id,
                validator,
                claimed: AtomicBool::new(false),
            })),
            server,
            method: method.to_owned(),
            call_id,
            original,
            request_state,
        })
    }

    pub fn method(&self) -> &'static str {
        "input_required"
    }

    pub fn request_id(&self) -> &McpInputRequestId {
        &self.request_id
    }

    pub fn sealed_state(&self) -> &str {
        &self.request_id.0.value
    }

    pub fn server(&self) -> &McpServerName {
        &self.server
    }

    pub fn resume(self, response: McpInputResponse) -> Result<McpRetryRequest, McpError> {
        if response.request_id != self.request_id {
            return Err(protocol("MRTR response identity mismatch"));
        }
        self.request_id
            .0
            .claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| protocol("MRTR answer was already consumed"))?;
        let mut original: Value = serde_json::from_slice(&self.original)?;
        let object = original
            .as_object_mut()
            .ok_or_else(|| invalid("MRTR original operation is not an object"))?;
        let mut responses = Map::new();
        responses.insert(self.call_id.clone(), response.answer);
        object.insert("inputResponses".to_owned(), Value::Object(responses));
        object.insert(
            "requestState".to_owned(),
            Value::String(
                String::from_utf8(self.request_state)
                    .map_err(|_| protocol("MRTR requestState is not UTF-8"))?,
            ),
        );
        Ok(McpRetryRequest {
            method: self.method,
            call_id: self.call_id,
            original: self.original,
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
        if request_id.0.claimed.load(Ordering::Acquire) {
            return Err(protocol("MRTR answer was already consumed"));
        }
        let answer: Value = serde_json::from_slice(answer.as_ref())?;
        validate_answer(&request_id.0.validator, &answer)?;
        Ok(Self {
            request_id: request_id.clone(),
            answer,
        })
    }
}

pub struct McpRetryRequest {
    method: String,
    call_id: String,
    original: Vec<u8>,
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

    pub fn original(&self) -> &[u8] {
        &self.original
    }
}

pub(crate) fn compile_schema(schema: &Value) -> Result<Validator, McpError> {
    jsonschema::validator_for(schema)
        .map_err(|error| invalid(format!("invalid MRTR JSON Schema: {error}")))
}

fn require_complete(
    result_type: Option<&rmcp::model::ResultType>,
    method: &str,
) -> Result<(), McpError> {
    if result_type.is_some_and(rmcp::model::ResultType::is_complete) {
        Ok(())
    } else {
        Err(protocol(format!(
            "current {method} result requires resultType complete"
        )))
    }
}

pub(crate) fn validate_answer(validator: &Validator, answer: &Value) -> Result<(), McpError> {
    let errors = validator
        .iter_errors(answer)
        .map(|error| format!("{}: {error}", error.instance_path()))
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(invalid(format!(
            "MRTR answer does not satisfy its schema: {}",
            errors.join("; ")
        )))
    }
}

fn validate_original(method: &str, call_id: &str, original: &[u8]) -> Result<(), McpError> {
    if !matches!(method, "tools/call" | "resources/read" | "prompts/get") || call_id.is_empty() {
        return Err(invalid("invalid MRTR operation identity"));
    }
    let original: Value = serde_json::from_slice(original)?;
    if !original.is_object() {
        return Err(invalid("MRTR original operation is not an object"));
    }
    Ok(())
}

fn derive_local_id(
    server: &McpServerName,
    method: &str,
    call_id: &str,
    schema: &Value,
    original: &[u8],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"ego:mcp:mrtr-correlation:v2");
    digest.update(server.as_str().as_bytes());
    digest.update(method.as_bytes());
    digest.update(call_id.as_bytes());
    digest.update(serde_json::to_vec(schema).unwrap_or_default());
    digest.update(original);
    format!("mrtr-{:x}", digest.finalize())
}

fn invalid(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::InvalidValue, message)
}

fn protocol(message: impl Into<String>) -> McpError {
    McpError::new(McpFailure::Protocol, message)
}
