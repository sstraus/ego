//! Protocol-neutral MCP projections owned by the core domain.
//!
//! Wire parsing remains in `ego-mcp`. These types retain the authority,
//! provenance and cache facts that must survive after protocol bytes have
//! been validated.

use std::fmt;
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::digest::sha256_hex;
use crate::generator::attempt::DeliveryCertainty;
use crate::permit::{InvocationDigest, ToolResult};
use crate::store::evidence::EvidenceRef;

use crate::mcp_task::valid_task_id;
pub use crate::mcp_task::{
    McpTaskLedger, McpTaskPoll, McpTaskPoller, McpTaskPollerHandle, McpTaskPort, McpTaskSeed,
    McpTaskSettlement, McpTaskStart, McpTaskTicket,
};

/// Maximum UTF-8 bytes admitted from one server's instructions.
/// Kept at the same 64 KiB bound as instruction files and skill bodies.
pub const FOREIGN_VISIBLE_BYTES: usize = 64 * 1024;
/// MCP tool text previews use 20 KiB; complete text lives in the session store.
pub const MCP_RESULT_VISIBLE_BYTES: usize = 20 * 1024;
/// Maximum bytes retained inline for one unsupported content block.
pub const FOREIGN_UNSUPPORTED_BYTES: usize = 32 * 1024;

/// What one admitted MCP server says about itself.
///
/// The `instructions` an MCP server may return from `initialize`: what it is
/// for, and how its tools should be preferred. ego has one immutable mixed
/// catalogue per generation and rules that allow or deny but never prefer, so
/// without this a server cannot explain itself at all — the only thing the
/// model learns about a foreign tool surface is the tool descriptions.
///
/// Bounded on the way in, and the bound is *recorded* rather than applied
/// silently: a shortened instruction reads exactly like a complete one, so a
/// server whose guidance was cut must be able to say so.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerInstructions {
    server: String,
    text: String,
    truncated: bool,
}

impl McpServerInstructions {
    /// Admit one server's instructions, bounded at [`FOREIGN_VISIBLE_BYTES`].
    pub fn new(server: impl Into<String>, text: &str) -> Self {
        let mut bounded = String::new();
        let mut truncated = false;
        append_bounded_text(&mut bounded, text, &mut truncated);
        Self {
            server: server.into(),
            text: bounded,
            truncated,
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the bound cut this server's instructions.
    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    Public,
    Private,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSensitivity {
    Public,
    Restricted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Resource,
    Prompt,
    Tool,
}

/// The exact foreign subject retained by one admitted MCP operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpSubject {
    Tool {
        server: String,
        name: String,
    },
    Resource {
        server: String,
        uri: String,
    },
    Prompt {
        server: String,
        name: String,
    },
    /// A server task that this session's own `tools/call` created. `tool` is
    /// the originating tool, so policy and display name the tool that earned
    /// the authority.
    Task {
        server: String,
        tool: String,
        task_id: String,
    },
}

impl McpSubject {
    pub fn server(&self) -> &str {
        match self {
            Self::Tool { server, .. }
            | Self::Resource { server, .. }
            | Self::Prompt { server, .. }
            | Self::Task { server, .. } => server,
        }
    }

    pub fn operation_name(&self) -> &str {
        match self {
            Self::Tool { name, .. } | Self::Prompt { name, .. } => name,
            Self::Task { tool, .. } => tool,
            Self::Resource { uri, .. } => uri,
        }
    }

    pub fn validate(&self) -> Result<(), McpProjectionError> {
        if !valid_identity(self.server())
            || self.operation_name().trim().is_empty()
            || self.operation_name().chars().any(char::is_control)
        {
            return Err(McpProjectionError::InvalidIdentity);
        }
        if let Self::Task { task_id, .. } = self
            && !valid_task_id(task_id)
        {
            return Err(McpProjectionError::InvalidIdentity);
        }
        Ok(())
    }
}

/// The selected protocol method. Lifecycle and management requests are not
/// admitted by this vocabulary and therefore cannot consume its authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpOperationKind {
    CallTool,
    ReadResource,
    GetPrompt,
    /// `tasks/get`: only for a task id recorded in an [`McpTaskTicket`].
    GetTask,
    /// `tasks/cancel`: only for a task id recorded in an [`McpTaskTicket`].
    CancelTask,
}

impl McpOperationKind {
    pub fn method(self) -> &'static str {
        match self {
            Self::CallTool => "tools/call",
            Self::ReadResource => "resources/read",
            Self::GetPrompt => "prompts/get",
            Self::GetTask => "tasks/get",
            Self::CancelTask => "tasks/cancel",
        }
    }

    pub fn matches(self, subject: &McpSubject) -> bool {
        matches!(
            (self, subject),
            (Self::CallTool, McpSubject::Tool { .. })
                | (Self::ReadResource, McpSubject::Resource { .. })
                | (Self::GetPrompt, McpSubject::Prompt { .. })
                | (Self::GetTask | Self::CancelTask, McpSubject::Task { .. })
        )
    }
}

/// A durable continuation was recorded before the foreign operation returned
/// control to the turn loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignInputRequired {
    continuation_id: String,
    question_request: crate::question::QuestionRequest,
}

impl ForeignInputRequired {
    pub fn new(
        continuation_id: impl Into<String>,
        question_request: crate::question::QuestionRequest,
    ) -> Result<Self, McpProjectionError> {
        let continuation_id = continuation_id.into();
        if !valid_identity(&continuation_id) || question_request.validate().is_err() {
            return Err(McpProjectionError::InvalidIdentity);
        }
        Ok(Self {
            continuation_id,
            question_request,
        })
    }

    pub fn continuation_id(&self) -> &str {
        &self.continuation_id
    }

    pub fn question_request(&self) -> &crate::question::QuestionRequest {
        &self.question_request
    }
}

/// The outcomes a foreign tool execution may expose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolExecution {
    Complete(ToolResult),
    InputRequired(ForeignInputRequired),
    /// The server made a task of the call. The model gets `result` now; the
    /// settled task reaches it later through the session's [`McpTaskPort`].
    TaskStarted(McpTaskStart),
}

/// A resume failure that distinguishes cancellation before the foreign retry
/// is claimed from errors that may have crossed the effect boundary.
#[derive(Debug)]
pub enum ForeignQuestionError {
    CancelledBeforeClaim,
    Failed(String),
}

impl From<String> for ForeignQuestionError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for ForeignQuestionError {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

/// Consumer-owned boundary for resuming one already-recorded foreign
/// continuation with a validated canonical form answer.
pub trait ForeignQuestionPort: Send + Sync {
    /// Terminal local settlement. It must never invoke the remote operation.
    fn cancel(&self, continuation_id: &str) -> Result<(), String>;

    /// Acknowledge a result or successor already committed by the turn.
    fn acknowledge_publication(&self, _continuation_id: &str) {}

    fn resume<'a>(
        &'a self,
        continuation_id: &'a str,
        canonical_json: &'a str,
        causality: crate::hook::HookCausality,
    ) -> BoxFuture<'a, Result<ToolExecution, ForeignQuestionError>>;
}

/// Session-owned factory for a foreign-question adapter bound to the exact
/// hook dispatcher, resume floor and effect authority of one turn.
pub trait McpRuntimeBinding: Send + Sync {
    /// User-visible context commands from one immutable admitted generation.
    fn context_commands(&self) -> Result<crate::turn::CapturedToolCatalogue, String> {
        crate::turn::CapturedToolCatalogue::new("empty", Vec::new()).map_err(|e| e.to_string())
    }

    /// Runtimes without an admitted foreign tool catalogue need no sealer.
    fn resume_sealer(&self) -> Option<Arc<crate::suspension::ResumeSealer>> {
        None
    }

    fn load_pending_foreign_policy(
        &self,
        continuation_id: &str,
    ) -> Result<Arc<crate::turn::GenerationPolicySnapshot>, String>;

    fn bind_pending_foreign_continuation(
        &self,
        continuation_id: &str,
        snapshot: Arc<crate::turn::GenerationPolicySnapshot>,
    ) -> Result<(), String>;

    fn hook_invocation_port<'a>(
        &self,
        policy: Arc<crate::permit::PolicyEngine>,
        effects: crate::turn::ToolEffectPorts<'a>,
    ) -> Result<Arc<dyn crate::hook::HookInvocationPort + 'a>, String>;

    fn foreign_question_port<'session, 'turn>(
        &self,
        runtime: crate::hook::SessionHookRuntime<'session>,
        floor: crate::suspension::ResumeFloor,
        effects: crate::turn::ToolEffectPorts<'turn>,
    ) -> Arc<dyn ForeignQuestionPort + 'turn>
    where
        'session: 'turn;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpAdmissionFingerprint {
    capture: String,
    operation: String,
    schema: String,
}

impl McpAdmissionFingerprint {
    pub fn new(
        capture: impl Into<String>,
        operation: impl Into<String>,
        schema: impl Into<String>,
    ) -> Result<Self, McpProjectionError> {
        let fingerprint = Self {
            capture: capture.into(),
            operation: operation.into(),
            schema: schema.into(),
        };
        if [
            &fingerprint.capture,
            &fingerprint.operation,
            &fingerprint.schema,
        ]
        .into_iter()
        .any(|part| !valid_identity(part))
        {
            return Err(McpProjectionError::InvalidFingerprint);
        }
        Ok(fingerprint)
    }

    pub fn capture(&self) -> &str {
        &self.capture
    }

    pub fn operation(&self) -> &str {
        &self.operation
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForeignContent {
    Text {
        text: String,
    },
    Image {
        image: crate::generator::types::ImageRef,
    },
    Unsupported {
        content_type: String,
        retained: Vec<u8>,
        original_bytes: u64,
        sha256: String,
    },
}

impl ForeignContent {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn unsupported(content_type: impl Into<String>, bytes: &[u8]) -> Self {
        let retained = bytes
            .get(..bytes.len().min(FOREIGN_UNSUPPORTED_BYTES))
            .unwrap_or(bytes)
            .to_vec();
        Self::Unsupported {
            content_type: content_type.into(),
            retained,
            original_bytes: bytes.len().try_into().unwrap_or(u64::MAX),
            sha256: sha256_hex(bytes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnsupportedContent {
    kind: String,
    original_bytes: u64,
    retained_bytes: u64,
    sha256: String,
}

impl UnsupportedContent {
    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn original_bytes(&self) -> u64 {
        self.original_bytes
    }

    pub fn retained_bytes(&self) -> u64 {
        self.retained_bytes
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationClassification {
    Completed,
    Cancelled,
    TimedOut,
    Failed,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignResult {
    server: String,
    tool: String,
    invocation_digest: String,
    admission: McpAdmissionFingerprint,
    result_type: String,
    ttl_ms: Option<u64>,
    cache_scope: Option<CacheScope>,
    visible_text: String,
    visible_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_path: Option<std::path::PathBuf>,
    // Owned only until publication. Never put the complete payload in events.
    #[serde(skip)]
    pending_output: Option<String>,
    // Bounded ordered model projection. Only text and validated references are
    // retained here; complete text and unsupported bytes remain evidence-owned.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    model_content: Vec<ForeignContent>,
    unsupported: Vec<UnsupportedContent>,
    evidence: EvidenceRef,
    delivery: DeliveryCertainty,
    cancellation: CancellationClassification,
    /// The server marked this complete tool result as an error (`isError`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    tool_error: bool,
}

impl ForeignResult {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        server: impl Into<String>,
        tool: impl Into<String>,
        invocation_digest: InvocationDigest,
        admission: McpAdmissionFingerprint,
        result_type: impl Into<String>,
        ttl_ms: Option<u64>,
        cache_scope: Option<CacheScope>,
        content: Vec<ForeignContent>,
        evidence: EvidenceRef,
        delivery: DeliveryCertainty,
        cancellation: CancellationClassification,
    ) -> Result<Self, McpProjectionError> {
        let server = server.into();
        let tool = tool.into();
        let result_type = result_type.into();
        if !valid_identity(&server) || !valid_identity(&tool) {
            return Err(McpProjectionError::InvalidIdentity);
        }
        if result_type.trim().is_empty() || result_type.chars().any(char::is_control) {
            return Err(McpProjectionError::MissingResultType);
        }

        let has_images = content
            .iter()
            .any(|block| matches!(block, ForeignContent::Image { .. }));
        let mut model_content = Vec::new();
        let mut model_text_bytes = 0;
        let mut visible_text = String::new();
        let mut unsupported = Vec::new();
        for block in content {
            match block {
                ForeignContent::Text { text } => {
                    if !visible_text.is_empty() {
                        visible_text.push('\n');
                    }
                    visible_text.push_str(&text);
                    if has_images && !text.is_empty() {
                        let remaining = MCP_RESULT_VISIBLE_BYTES.saturating_sub(model_text_bytes);
                        // Omit a whole overflowing block rather than cutting a
                        // secret before the ordinary masking boundary sees it.
                        let text_bytes = text.len().saturating_add(usize::from(matches!(
                            model_content.last(),
                            Some(ForeignContent::Text { .. })
                        )));
                        let text = if text_bytes <= remaining {
                            model_text_bytes = model_text_bytes.saturating_add(text_bytes);
                            text
                        } else {
                            "[MCP text block omitted from ordered context; see the truncated result output]".into()
                        };
                        if let Some(ForeignContent::Text { text: previous }) =
                            model_content.last_mut()
                        {
                            // All useful text contributes to the shared budget;
                            // repeated overflow diagnostics need not be repeated.
                            if !text.starts_with("[MCP text block omitted") {
                                previous.push('\n');
                                previous.push_str(&text);
                            }
                        } else {
                            model_content.push(ForeignContent::text(text));
                        }
                    }
                }
                ForeignContent::Image { image } => {
                    model_content.push(ForeignContent::Image { image })
                }
                ForeignContent::Unsupported {
                    content_type,
                    retained,
                    original_bytes,
                    sha256,
                } => unsupported.push(UnsupportedContent {
                    kind: content_type,
                    original_bytes,
                    retained_bytes: retained.len().try_into().unwrap_or(u64::MAX),
                    sha256,
                }),
            }
        }
        let pending_output = (visible_text.len() > MCP_RESULT_VISIBLE_BYTES)
            .then(|| std::mem::take(&mut visible_text));
        let visible_truncated = pending_output.is_some();
        if let Some(text) = &pending_output {
            visible_text = render_mcp_preview(text, None);
        }
        Ok(Self {
            server,
            tool,
            invocation_digest: invocation_digest.as_str().to_owned(),
            admission,
            result_type,
            ttl_ms,
            cache_scope,
            visible_text,
            visible_truncated,
            output_path: None,
            pending_output,
            model_content,
            unsupported,
            evidence,
            delivery,
            cancellation,
            tool_error: false,
        })
    }

    /// Record the server's `isError` flag of a complete tool result.
    #[must_use]
    pub fn with_tool_error(mut self, tool_error: bool) -> Self {
        self.tool_error = tool_error;
        self
    }

    /// True when the server reported this tool result as an error.
    pub const fn tool_error(&self) -> bool {
        self.tool_error
    }

    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn tool(&self) -> &str {
        &self.tool
    }
    pub fn invocation_digest(&self) -> &str {
        &self.invocation_digest
    }
    pub fn admission_capture(&self) -> &str {
        self.admission.capture()
    }
    pub fn admission(&self) -> &McpAdmissionFingerprint {
        &self.admission
    }
    pub fn result_type(&self) -> &str {
        &self.result_type
    }
    pub fn ttl_ms(&self) -> Option<u64> {
        self.ttl_ms
    }
    pub fn cache_scope(&self) -> Option<CacheScope> {
        self.cache_scope
    }
    pub(crate) fn mask_secrets(&mut self, values: &crate::secrets::SecretValues) {
        for block in &mut self.model_content {
            if let ForeignContent::Text { text } = block {
                *text = values.mask(text);
            }
        }
        if let Some(text) = &mut self.pending_output {
            *text = values.mask(text);
            self.visible_text = render_mcp_preview(text, None);
        } else {
            self.visible_text = values.mask(&self.visible_text);
        }
    }

    /// Ordered, bounded text/image context following the correlated tool result.
    pub fn model_content(&self) -> &[ForeignContent] {
        &self.model_content
    }

    /// Untrusted tool output with ordered references to its separately hydrated images.
    pub fn model_text(&self) -> String {
        if self.model_content.is_empty() {
            return self.visible_text.clone();
        }
        let mut output = String::new();
        let mut image_index = 0;
        for block in &self.model_content {
            let text = match block {
                ForeignContent::Text { text } if !text.is_empty() => text.clone(),
                ForeignContent::Image { .. } => {
                    image_index += 1;
                    format!("[image {image_index}]")
                }
                _ => continue,
            };
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&text);
        }
        if self.visible_truncated {
            output.push_str("\n[MCP text truncated. ");
            output.push_str(&mcp_output_location(self.output_path.as_deref()));
            output.push(']');
        }
        output
    }

    pub fn visible_text(&self) -> &str {
        &self.visible_text
    }
    pub const fn visible_limit(&self) -> usize {
        MCP_RESULT_VISIBLE_BYTES
    }

    /// The complete output file, when its session publication succeeded.
    pub fn output_path(&self) -> Option<&std::path::Path> {
        self.output_path.as_deref()
    }

    pub(crate) fn spill_output(
        &mut self,
        store: Option<&dyn crate::store::evidence::EvidenceStore>,
        run: crate::evidence::RunInvocationId,
        call: crate::event::id::ItemId,
    ) {
        let Some(text) = self.pending_output.take() else {
            return;
        };
        self.output_path =
            store.and_then(|store| store.spill_mcp_output(run, call, text.as_bytes()).ok());
        self.visible_text = render_mcp_preview(&text, self.output_path.as_deref());
    }
    pub fn visible_truncated(&self) -> bool {
        self.visible_truncated
    }
    pub fn unsupported(&self) -> &[UnsupportedContent] {
        &self.unsupported
    }
    pub fn evidence_reference(&self) -> &EvidenceRef {
        &self.evidence
    }
    pub fn delivery(&self) -> DeliveryCertainty {
        self.delivery
    }
    pub fn cancellation(&self) -> CancellationClassification {
        self.cancellation
    }
}

fn render_mcp_preview(text: &str, path: Option<&std::path::Path>) -> String {
    let mut boundary = text.len().min(MCP_RESULT_VISIBLE_BYTES);
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    if let Some(newline) = text[..boundary].rfind('\n') {
        boundary = newline + 1;
    }
    let preview = &text[..boundary];
    let separator = if preview.ends_with('\n') { "" } else { "\n" };
    let saved = mcp_output_location(path);
    format!(
        "{preview}{separator}[MCP output truncated: showing first {boundary} bytes of {} bytes. {saved}]",
        text.len()
    )
}

fn mcp_output_location(path: Option<&std::path::Path>) -> String {
    match path {
        Some(path) => format!(
            "Full output written to: {}. Use read with a files array containing this path to read it.",
            path.display()
        ),
        None => "Full output could not be saved.".to_owned(),
    }
}

fn append_bounded_text(target: &mut String, text: &str, truncated: &mut bool) {
    if target.len() >= FOREIGN_VISIBLE_BYTES {
        *truncated |= !text.is_empty();
        return;
    }
    if !target.is_empty() {
        target.push('\n');
    }
    let remaining = FOREIGN_VISIBLE_BYTES.saturating_sub(target.len());
    if text.len() <= remaining {
        target.push_str(text);
        return;
    }
    let mut boundary = remaining.min(text.len());
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    target.push_str(&text[..boundary]);
    *truncated = true;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextEntry {
    kind: ContextKind,
    server: String,
    source: String,
    name: String,
    description: String,
    arguments: Vec<String>,
    sensitivity: ContextSensitivity,
    cache_scope: CacheScope,
    ttl_ms: Option<u64>,
    revision: String,
    admission_capture: Option<String>,
}

impl ContextEntry {
    pub fn resource(
        server: impl Into<String>,
        source: impl Into<String>,
        name: impl Into<String>,
        sensitivity: ContextSensitivity,
        cache_scope: CacheScope,
        revision: impl Into<String>,
    ) -> Result<Self, ContextError> {
        Self::new(
            ContextKind::Resource,
            server,
            source,
            name,
            "",
            Vec::new(),
            sensitivity,
            cache_scope,
            revision,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prompt(
        server: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        arguments: Vec<String>,
        sensitivity: ContextSensitivity,
        cache_scope: CacheScope,
        revision: impl Into<String>,
    ) -> Result<Self, ContextError> {
        let name = name.into();
        Self::new(
            ContextKind::Prompt,
            server,
            name.clone(),
            name,
            description,
            arguments,
            sensitivity,
            cache_scope,
            revision,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        kind: ContextKind,
        server: impl Into<String>,
        source: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        arguments: Vec<String>,
        sensitivity: ContextSensitivity,
        cache_scope: CacheScope,
        revision: impl Into<String>,
    ) -> Result<Self, ContextError> {
        let entry = Self {
            kind,
            server: server.into(),
            source: source.into(),
            name: name.into(),
            description: description.into(),
            arguments,
            sensitivity,
            cache_scope,
            ttl_ms: None,
            revision: revision.into(),
            admission_capture: None,
        };
        if !valid_identity(&entry.server)
            || entry.source.trim().is_empty()
            || entry.name.trim().is_empty()
            || entry.revision.trim().is_empty()
            || entry
                .arguments
                .iter()
                .any(|argument| argument.trim().is_empty())
        {
            return Err(ContextError::InvalidEntry);
        }
        Ok(entry)
    }

    pub fn with_cache_ttl(mut self, ttl_ms: u64) -> Self {
        self.ttl_ms = Some(ttl_ms);
        self
    }
    pub fn with_admission_capture(
        mut self,
        capture: impl Into<String>,
    ) -> Result<Self, ContextError> {
        let capture = capture.into();
        if !valid_identity(&capture) {
            return Err(ContextError::InvalidEntry);
        }
        self.admission_capture = Some(capture);
        Ok(self)
    }
    pub fn kind(&self) -> ContextKind {
        self.kind
    }
    pub fn server(&self) -> &str {
        &self.server
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn description(&self) -> &str {
        &self.description
    }
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
    pub fn sensitivity(&self) -> ContextSensitivity {
        self.sensitivity
    }
    pub fn cache_scope(&self) -> CacheScope {
        self.cache_scope
    }
    pub fn ttl_ms(&self) -> Option<u64> {
        self.ttl_ms
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn admission_capture(&self) -> Option<&str> {
        self.admission_capture.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContextSnapshot(Vec<ContextEntry>);

impl ContextSnapshot {
    pub fn new(entries: Vec<ContextEntry>) -> Result<Self, ContextError> {
        let mut identities = std::collections::HashSet::with_capacity(entries.len());
        if entries.iter().any(|entry| {
            !identities.insert((entry.kind, entry.server.clone(), entry.source.clone()))
        }) {
            return Err(ContextError::DuplicateEntry);
        }
        Ok(Self(entries))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, ContextEntry> {
        self.0.iter()
    }
    pub fn entries(&self) -> &[ContextEntry] {
        &self.0
    }
}

pub trait ContextSource: Send + Sync {
    fn capture(&self) -> Result<ContextSnapshot, ContextError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpProjectionError {
    InvalidIdentity,
    InvalidFingerprint,
    MissingResultType,
    InvalidContent,
}

impl fmt::Display for McpProjectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidIdentity => "invalid MCP identity",
            Self::InvalidFingerprint => "invalid MCP admission fingerprint",
            Self::MissingResultType => "MCP resultType is missing or invalid",
            Self::InvalidContent => "invalid MCP content projection",
        })
    }
}

impl std::error::Error for McpProjectionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextError {
    InvalidEntry,
    DuplicateEntry,
    Unavailable(String),
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEntry => formatter.write_str("invalid context entry"),
            Self::DuplicateEntry => formatter.write_str("duplicate context entry"),
            Self::Unavailable(message) => {
                write!(formatter, "context source unavailable: {message}")
            }
        }
    }
}

impl std::error::Error for ContextError {}

fn valid_identity(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

/// Correlate one durable tool call across MCP dispatch and ACP history replay.
///
/// SHA-256 input is `ego/tuic-call-id/v1\0`, followed by three UTF-8 fields,
/// each prefixed by its byte length as a big-endian u64: canonical session id,
/// identity kind (`provider` or `item`), and identity value. The result is
/// lowercase hex. Legacy records without a provider call id use the durable
/// ego item id, in a separate namespace; no new identity is persisted.
/// This identifies a call, not permission to retry an uncertain effect.
pub fn tool_call_correlation_id(
    session: &str,
    item: crate::event::id::ItemId,
    provider_call_id: Option<&str>,
) -> String {
    use sha2::{Digest, Sha256};
    let item_id = item.to_string();
    let (kind, value) = match provider_call_id {
        Some(id) => ("provider", id),
        None => ("item", item_id.as_str()),
    };
    let mut hash = Sha256::new();
    hash.update(b"ego/tuic-call-id/v1\0");
    for field in [session, kind, value] {
        hash.update((field.len() as u64).to_be_bytes());
        hash.update(field.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

#[cfg(test)]
mod correlation_tests {
    use super::tool_call_correlation_id;
    use crate::event::id::ItemId;

    // Catches: missing session scoping, item fallback drift, ambiguous concatenation,
    // or accidentally hashing the transient item instead of the provider call id.
    #[test]
    fn durable_tool_correlation_vectors_and_identity_boundaries() {
        let session = "00000000-0000-7000-8000-000000000001";
        let item: ItemId = "00000000-0000-7000-8000-000000000002".parse().unwrap();
        let other: ItemId = "00000000-0000-7000-8000-000000000003".parse().unwrap();
        let provider = tool_call_correlation_id(session, item, Some("call-17"));
        // Independent Python hashlib SHA-256 vectors of the documented byte contract.
        assert_eq!(
            provider,
            "aa7602e8fee6fe7c6f275f4ae5b7f5140d98b639f7f18c4f679ef99717c70d81"
        );
        let legacy = tool_call_correlation_id(session, item, None);
        assert_eq!(
            legacy,
            "41b2f3110aa4c4f4aa0ad353528ec86f7ca2a0d3ebe15f3e94b1c2465337c55a"
        );
        assert_eq!(
            provider,
            tool_call_correlation_id(session, other, Some("call-17"))
        );
        assert_ne!(
            provider,
            tool_call_correlation_id("another-session", item, Some("call-17"))
        );
        assert_ne!(
            provider,
            tool_call_correlation_id(session, item, Some("call-18"))
        );
        assert_ne!(legacy, tool_call_correlation_id(session, other, None));
        assert_ne!(
            legacy,
            tool_call_correlation_id(session, item, Some(&item.to_string()))
        );
        assert_ne!(legacy, tool_call_correlation_id(session, item, Some("")));
        assert_ne!(
            tool_call_correlation_id("ab", item, Some("c")),
            tool_call_correlation_id("a", item, Some("bc"))
        );
    }
}
