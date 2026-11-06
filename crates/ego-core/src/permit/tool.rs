//! Typed tool descriptions and guarded execution ports.

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::PathResolver;
use super::fs::{FsError, GuardedIo, IoDenied};
use super::grant::PermitDenied;
use super::path::CanonPath;
use super::prepared_call::PreparedCall;
use super::spawn::ProcessError;
use crate::generator::attempt::DeliveryCertainty;
use crate::mcp::{ForeignResult, ToolExecution};
use crate::store::evidence::{EvidenceError, EvidencePage, EvidenceRef};
use crate::suspension::ContinuationError;

/// Maximum UTF-8 bytes exposed for one failed-tool diagnostic.
pub const MAX_TOOL_ERROR_DIAGNOSTIC_BYTES: usize = 4 * 1024;

/// A tool's identity and parameter schema, as shown to the model.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub schema: schemars::Schema,
}

/// Read-only canonicalization context available before policy authorization.
pub struct PrepareCtx<'a> {
    pub cwd: CanonPath,
    pub resolver: &'a dyn PathResolver,
}

/// Why a `Tool` call failed, before or during execution.
///
/// `Denied` and `Fs` stay separate all the way to the model: a model told it
/// was refused should stop asking, and a model told the file is not there
/// should look somewhere else. One collapsed "it didn't work" produces the
/// wrong next move in both cases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// `prepare`'s params didn't match the tool's schema.
    InvalidParams(String),
    /// `execute` hit an operation the `Permit` doesn't cover.
    Denied(PermitDenied),
    /// The filesystem refused or could not find something. Reachable from
    /// `prepare` too: resolving a path the model named is the first thing a
    /// read-only tool does, and a missing file fails there, before there is
    /// anything to authorise.
    Fs(FsError),
    /// Complete output could not be stored or verified. This is never
    /// downgraded to a successful preview: a prefix is not evidence of the
    /// output a tool actually produced.
    Evidence(EvidenceError),
    /// The admitted process did not complete successfully.
    Process(ProcessError),
    /// Durable MCP continuation authority could not be prepared or recorded.
    Continuation(ContinuationError),
    /// A builtin failed after producing durable per-file evidence.
    WriteFailed(Box<ToolResult>),
    /// A foreign effect failed after its delivery certainty was known.
    Foreign {
        message: String,
        delivery: DeliveryCertainty,
    },
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::InvalidParams(msg) => write!(f, "invalid params: {msg}"),
            ToolError::Denied(denied) => write!(f, "{denied}"),
            ToolError::Fs(err) => write!(f, "{err}"),
            ToolError::Evidence(err) => write!(f, "{err}"),
            ToolError::Process(err) => write!(f, "{err}"),
            ToolError::Continuation(err) => write!(f, "{err}"),
            ToolError::Foreign { message, .. } => f.write_str(message),
            ToolError::WriteFailed(result) => f.write_str(&result.model_text()),
        }
    }
}

impl std::error::Error for ToolError {}

impl ToolError {
    pub fn foreign(message: impl Into<String>, delivery: DeliveryCertainty) -> Self {
        Self::Foreign {
            message: message.into(),
            delivery,
        }
    }

    pub fn delivery(&self) -> Option<DeliveryCertainty> {
        match self {
            Self::Foreign { delivery, .. } => Some(*delivery),
            Self::InvalidParams(_)
            | Self::Denied(_)
            | Self::Fs(_)
            | Self::Evidence(_)
            | Self::Process(_)
            | Self::Continuation(_)
            | Self::WriteFailed(_) => None,
        }
    }

    pub fn into_failed_result(self) -> ToolResult {
        match self {
            Self::Process(error) => error.into_failed_result(),
            Self::WriteFailed(result) => *result,
            error => ToolResult::builtin(bounded_diagnostic(&error.to_string())),
        }
    }
}

pub(super) fn bounded_diagnostic(message: &str) -> String {
    if message.len() <= MAX_TOOL_ERROR_DIAGNOSTIC_BYTES {
        return message.to_string();
    }
    let mut end = MAX_TOOL_ERROR_DIAGNOSTIC_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

impl From<PermitDenied> for ToolError {
    fn from(denied: PermitDenied) -> Self {
        ToolError::Denied(denied)
    }
}

impl From<FsError> for ToolError {
    fn from(err: FsError) -> Self {
        ToolError::Fs(err)
    }
}

impl From<IoDenied> for ToolError {
    fn from(denied: IoDenied) -> Self {
        match denied {
            IoDenied::Permit(denied) => ToolError::Denied(denied),
            IoDenied::Fs(err) => ToolError::Fs(err),
        }
    }
}

impl From<EvidenceError> for ToolError {
    fn from(error: EvidenceError) -> Self {
        Self::Evidence(error)
    }
}

impl From<ProcessError> for ToolError {
    fn from(error: ProcessError) -> Self {
        Self::Process(error)
    }
}

impl From<ContinuationError> for ToolError {
    fn from(error: ContinuationError) -> Self {
        Self::Continuation(error)
    }
}

/// Which backend-owned pipe produced a block of process evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProvenance {
    Stdout,
    Stderr,
}

/// Encoding used to render arbitrary stored bytes into model-visible text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolOutputEncoding {
    Utf8,
    Base64,
}

impl std::fmt::Display for StreamProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        })
    }
}

/// How the text in one result block was obtained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ToolOutputProvenance {
    /// A builtin produced the complete text directly.
    Builtin,
    /// A backend-owned process pipe was drained completely. `truncated`
    /// describes only the preview; the referenced bytes remain complete.
    Process {
        stream: StreamProvenance,
        encoding: ToolOutputEncoding,
        truncated: bool,
        omitted_bytes: u64,
    },
    /// One fixed-size verified page from a session evidence object.
    EvidencePage {
        encoding: ToolOutputEncoding,
        offset: u64,
        total: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        next: Option<u64>,
        eof: bool,
    },
}

impl ToolOutputProvenance {
    pub fn head_tail(&self) -> bool {
        matches!(
            self,
            Self::Process {
                truncated: true,
                ..
            }
        )
    }
}

/// One ordered, model-visible text block and the complete evidence behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolContentBlock {
    pub text: String,
    pub provenance: ToolOutputProvenance,
    /// Always present for a process pipe, including an empty or small one.
    /// Builtins that already returned their complete text need no blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output: Option<EvidenceRef>,
}

impl ToolContentBlock {
    pub fn builtin(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            provenance: ToolOutputProvenance::Builtin,
            full_output: None,
        }
    }

    pub fn process(
        stream: StreamProvenance,
        bytes: &[u8],
        preview_bytes: usize,
        full_output: EvidenceRef,
    ) -> Self {
        let (text, omitted_bytes, encoding) = head_tail_preview(bytes, preview_bytes, &full_output);
        Self {
            text,
            provenance: ToolOutputProvenance::Process {
                stream,
                encoding,
                truncated: omitted_bytes != 0,
                omitted_bytes,
            },
            full_output: Some(full_output),
        }
    }

    /// Build a process block from the bounded head and tail retained while a
    /// stream was drained. `full_output.bytes` remains the complete length.
    pub fn process_preview(
        stream: StreamProvenance,
        head: &[u8],
        tail: &[u8],
        full_output: EvidenceRef,
    ) -> Self {
        let retained = head.len().saturating_add(tail.len()) as u64;
        let omitted_bytes = full_output.bytes.saturating_sub(retained);
        let mut preview = Vec::with_capacity(head.len().saturating_add(tail.len()));
        preview.extend_from_slice(head);
        preview.extend_from_slice(tail);
        let encoding = if std::str::from_utf8(head).is_ok() && std::str::from_utf8(tail).is_ok() {
            ToolOutputEncoding::Utf8
        } else {
            ToolOutputEncoding::Base64
        };
        let text = if omitted_bytes == 0 {
            render_bytes(&preview, encoding)
        } else if encoding == ToolOutputEncoding::Base64 {
            format!(
                "[base64 head+tail preview; {omitted_bytes} bytes omitted between retained segments]\n{}\n[read_tool_result {full_output}]",
                base64(&preview)
            )
        } else {
            format!(
                "{}\n\n[... {omitted_bytes} bytes omitted; read_tool_result {full_output} ...]\n\n{}",
                render_bytes(head, encoding),
                render_bytes(tail, encoding)
            )
        };
        Self {
            text,
            provenance: ToolOutputProvenance::Process {
                stream,
                encoding,
                truncated: omitted_bytes != 0,
                omitted_bytes,
            },
            full_output: Some(full_output),
        }
    }

    /// Render one bounded evidence page without lossy byte conversion.
    pub fn evidence_page(page: EvidencePage) -> Self {
        let encoding = encoding_of(&page.bytes);
        let text = render_bytes(&page.bytes, encoding);
        let eof = page.next_offset.is_none();
        Self {
            text,
            provenance: ToolOutputProvenance::EvidencePage {
                encoding,
                offset: page.offset,
                total: page.reference.bytes,
                next: page.next_offset,
                eof,
            },
            full_output: Some(page.reference),
        }
    }

    pub fn is_bounded(&self) -> bool {
        true
    }

    pub fn backend_owned_pipe(&self) -> bool {
        matches!(self.provenance, ToolOutputProvenance::Process { .. })
    }

    pub fn provenance(&self) -> &ToolOutputProvenance {
        &self.provenance
    }

    pub fn evidence_reference(&self) -> Option<&EvidenceRef> {
        self.full_output.as_ref()
    }

    pub fn is_truncated(&self) -> bool {
        self.provenance.head_tail()
    }

    pub fn preview_is_head_and_tail(&self) -> bool {
        self.provenance.head_tail()
    }
}

/// A tool's ordered evidence blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<ToolContentBlock>,
    provenance: ToolResultProvenance,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    line_change: Option<LineChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    before_image: Option<Box<BeforeImage>>,
    /// The session file checkpoint a builtin write created, so a rewind can
    /// find exactly the file changes after a turn. Journaled, never sent to
    /// the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checkpoint: Option<Box<str>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    file_writes: Vec<FileWriteEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileWriteEvidence {
    pub path: String,
    pub completed: bool,
    pub before_image: Option<BeforeImage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_image: Option<String>,
    pub checkpoint: Option<String>,
    pub possibly_modified: bool,
    pub summary: String,
    pub notice: Option<String>,
}

/// Most bytes of a replaced file kept as a before image. A larger file is
/// recorded as [`BeforeImage::Omitted`] so a journal never carries a second
/// copy of a large file.
pub const BEFORE_IMAGE_LIMIT: usize = 64 * 1024;

/// What a file held before a builtin write replaced it. For the terminal
/// diff only: it is journaled with the result and never sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
pub enum BeforeImage {
    /// The file did not exist.
    New,
    /// The UTF-8 text the file held, at most [`BEFORE_IMAGE_LIMIT`] bytes.
    Text(String),
    /// Too large, not text, or a protected credential file.
    Omitted,
}

/// Lines observed in one successful builtin file edit. Missing means unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineChange {
    pub added: u64,
    pub removed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultProvenance {
    Builtin,
    Foreign { result: Box<ForeignResult> },
}

impl ToolResult {
    /// Mask disclosure text without changing tool identity or evidence references.
    pub fn mask_secrets(&mut self, values: &crate::secrets::SecretValues) {
        for block in &mut self.content {
            block.text = values.mask(&block.text);
        }
        if let Some(BeforeImage::Text(text)) = self.before_image.as_deref_mut() {
            *text = values.mask(text);
        }
        for file in &mut self.file_writes {
            if let Some(BeforeImage::Text(text)) = &mut file.before_image {
                *text = values.mask(text);
            }
            if let Some(text) = &mut file.after_image {
                *text = values.mask(text);
            }
            file.path = values.mask(&file.path);
            file.summary = values.mask(&file.summary);
            if let Some(notice) = &mut file.notice {
                *notice = values.mask(notice);
            }
        }
        if let ToolResultProvenance::Foreign { result } = &mut self.provenance {
            result.mask_secrets(values);
        }
    }

    pub fn builtin(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContentBlock::builtin(text)],
            provenance: ToolResultProvenance::Builtin,
            line_change: None,
            before_image: None,
            checkpoint: None,
            file_writes: Vec::new(),
        }
    }

    pub fn with_file_writes(mut self, files: Vec<FileWriteEvidence>) -> Self {
        self.file_writes = files;
        self
    }

    pub fn file_writes(&self) -> &[FileWriteEvidence] {
        &self.file_writes
    }

    pub fn with_line_change(mut self, change: LineChange) -> Self {
        self.line_change = Some(change);
        self
    }

    pub fn line_change(&self) -> Option<LineChange> {
        self.line_change
    }

    pub fn with_before_image(mut self, image: BeforeImage) -> Self {
        self.before_image = Some(Box::new(image));
        self
    }

    pub fn before_image(&self) -> Option<&BeforeImage> {
        self.before_image.as_deref()
    }

    pub fn with_checkpoint(mut self, checkpoint: Option<String>) -> Self {
        self.checkpoint = checkpoint.map(String::into_boxed_str);
        self
    }

    pub fn checkpoint(&self) -> Option<&str> {
        self.checkpoint.as_deref()
    }

    pub fn new(content: Vec<ToolContentBlock>) -> Self {
        Self {
            content,
            provenance: ToolResultProvenance::Builtin,
            line_change: None,
            before_image: None,
            checkpoint: None,
            file_writes: Vec::new(),
        }
    }

    pub fn from_foreign(result: ForeignResult) -> Self {
        let content = vec![ToolContentBlock::builtin(result.visible_text().to_owned())];
        Self {
            content,
            provenance: ToolResultProvenance::Foreign {
                result: Box::new(result),
            },
            line_change: None,
            before_image: None,
            checkpoint: None,
            file_writes: Vec::new(),
        }
    }

    /// Publish oversized MCP text before exposing or journaling the result.
    /// An unavailable store or failed write keeps a preview and a failure notice.
    pub fn spill_mcp_output(
        &mut self,
        store: Option<&dyn crate::store::evidence::EvidenceStore>,
        run: crate::evidence::RunInvocationId,
        call: crate::event::id::ItemId,
    ) {
        if let ToolResultProvenance::Foreign { result } = &mut self.provenance {
            result.spill_output(store, run, call);
            self.content = vec![ToolContentBlock::builtin(result.visible_text().to_owned())];
        }
    }

    pub fn provenance(&self) -> &ToolResultProvenance {
        &self.provenance
    }

    /// True when the complete result was produced in memory and carries no
    /// evidence or external-effect provenance.
    pub fn is_pure(&self) -> bool {
        matches!(self.provenance, ToolResultProvenance::Builtin)
            && self.content.iter().all(|block| {
                matches!(block.provenance, ToolOutputProvenance::Builtin)
                    && block.full_output.is_none()
            })
    }

    /// The single textual answer provider protocols accept for a tool call.
    /// Provenance stays explicit when several process streams are projected
    /// into that one protocol field.
    pub fn model_text(&self) -> String {
        let one_builtin = self.content.as_slice();
        if let [
            ToolContentBlock {
                text,
                provenance: ToolOutputProvenance::Builtin,
                ..
            },
        ] = one_builtin
        {
            return text.clone();
        }

        self.content
            .iter()
            .map(|block| match &block.provenance {
                ToolOutputProvenance::Builtin => block.text.clone(),
                ToolOutputProvenance::Process { stream, .. } => {
                    format!("[{stream}]\n{}", block.text)
                }
                ToolOutputProvenance::EvidencePage { .. } => block.text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

fn head_tail_preview(
    bytes: &[u8],
    budget: usize,
    reference: &EvidenceRef,
) -> (String, u64, ToolOutputEncoding) {
    if bytes.len() <= budget {
        let encoding = encoding_of(bytes);
        return (render_bytes(bytes, encoding), 0, encoding);
    }

    let requested_head = budget / 2;
    let requested_tail = budget.saturating_sub(requested_head);
    let (head_len, tail_start) = match std::str::from_utf8(bytes) {
        Ok(text) => {
            let mut head = requested_head;
            while head > 0 && !text.is_char_boundary(head) {
                head -= 1;
            }
            let mut tail = bytes.len().saturating_sub(requested_tail);
            while tail < bytes.len() && !text.is_char_boundary(tail) {
                tail += 1;
            }
            (head, tail.max(head))
        }
        Err(_) => (requested_head, bytes.len().saturating_sub(requested_tail)),
    };
    let omitted = tail_start.saturating_sub(head_len) as u64;
    let retained = [&bytes[..head_len], &bytes[tail_start..]].concat();
    let encoding = encoding_of(&retained);
    if encoding == ToolOutputEncoding::Base64 {
        return (
            format!(
                "[base64 head+tail preview; {omitted} bytes omitted between retained segments]\n{}\n[read_tool_result {reference}]",
                base64(&retained)
            ),
            omitted,
            encoding,
        );
    }
    (
        format!(
            "{}\n\n[... {omitted} bytes omitted; read_tool_result {reference} ...]\n\n{}",
            render_bytes(&bytes[..head_len], encoding),
            render_bytes(&bytes[tail_start..], encoding)
        ),
        omitted,
        encoding,
    )
}

fn encoding_of(bytes: &[u8]) -> ToolOutputEncoding {
    if std::str::from_utf8(bytes).is_ok() {
        ToolOutputEncoding::Utf8
    } else {
        ToolOutputEncoding::Base64
    }
}

fn render_bytes(bytes: &[u8], encoding: ToolOutputEncoding) -> String {
    match encoding {
        ToolOutputEncoding::Utf8 => std::str::from_utf8(bytes)
            .expect("encoding was checked")
            .to_string(),
        ToolOutputEncoding::Base64 => format!("[base64]\n{}", base64(bytes)),
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        output.push(ALPHABET[((word >> 18) & 0x3f) as usize] as char);
        output.push(ALPHABET[((word >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[((word >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(word & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

/// Preparation describes exact requested effects without executing them.
///
/// `execute` receives [`GuardedIo`], never a raw filesystem or separate
/// permit. The grant stays inside `GuardedFs`, so callers cannot mispair
/// a tool call with another call's permit.
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    /// Explicit operator admission of provider-executed paid search.
    fn admits_hosted_search(&self) -> bool {
        false
    }

    /// Configured local executor used only when the resolved provider is non-native.
    fn local_search_tool(&self) -> Option<std::sync::Arc<dyn Tool>> {
        None
    }

    fn prepare(&self, params: Value, ctx: &PrepareCtx<'_>) -> Result<PreparedCall, ToolError>;

    fn execute<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a;
}

/// The typed half of a builtin tool: params with a `schemars`-derived
/// schema instead of hand-written JSON Schema and manual `Value` parsing.
pub trait TypedTool: Send + Sync {
    type Params: JsonSchema + DeserializeOwned;

    fn name(&self) -> &str;
    fn description(&self) -> &str;

    fn admits_hosted_search(&self) -> bool {
        false
    }

    /// Configured local executor used only when the resolved provider is non-native.
    fn local_search_tool(&self) -> Option<std::sync::Arc<dyn Tool>> {
        None
    }

    fn prepare_typed(
        &self,
        params: Self::Params,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError>;

    fn execute<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>
    where
        'io: 'a;
}

/// Tool execution receives only guarded effect adapters.
pub struct Typed<T>(pub T);

impl<T: TypedTool> Tool for Typed<T> {
    fn local_search_tool(&self) -> Option<std::sync::Arc<dyn Tool>> {
        self.0.local_search_tool()
    }

    fn admits_hosted_search(&self) -> bool {
        self.0.admits_hosted_search()
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: self.0.name().to_string(),
            description: self.0.description().to_string(),
            schema: schemars::schema_for!(T::Params),
        }
    }

    fn prepare(&self, params: Value, ctx: &PrepareCtx<'_>) -> Result<PreparedCall, ToolError> {
        let params: T::Params = serde_json::from_value(params)
            .map_err(|err| ToolError::InvalidParams(err.to_string()))?;
        self.0.prepare_typed(params, ctx)
    }

    fn execute<'a, 'io>(
        &'a self,
        call: PreparedCall,
        io: &'a dyn GuardedIo<'io>,
    ) -> BoxFuture<'a, Result<ToolExecution, ToolError>>
    where
        'io: 'a,
    {
        Box::pin(async move { self.0.execute(call, io).await.map(ToolExecution::Complete) })
    }
}

#[cfg(test)]
mod line_change_tests {
    use super::*;

    // Catches: file-write images or operator evidence bypass secret masking before journaling.
    #[test]
    fn file_write_evidence_masks_secrets_in_journal_and_preserves_context() {
        let secret = "secret-token-42";
        let mut result = ToolResult::builtin(format!("output {secret}")).with_file_writes(vec![
            FileWriteEvidence {
                path: format!("/workspace/{secret}/file.txt"),
                completed: true,
                before_image: Some(BeforeImage::Text(format!("before {secret} retained"))),
                after_image: Some(format!("after {secret} retained")),
                checkpoint: None,
                possibly_modified: false,
                summary: format!("summary {secret} retained"),
                notice: Some(format!("notice {secret} retained")),
            },
        ]);
        result.mask_secrets(&crate::secrets::SecretValues::new([secret
            .as_bytes()
            .to_vec()]));
        let journal = serde_json::to_string(&result).unwrap();
        assert!(!journal.contains(secret), "secret leaked in {journal}");
        for context in [
            "/workspace/",
            "file.txt",
            "before ",
            "after ",
            "summary ",
            "notice ",
            "retained",
            "output ",
        ] {
            assert!(
                journal.contains(context),
                "missing {context:?} in {journal}"
            );
        }
        let file = &result.file_writes()[0];
        assert!(!format!("{file:?}").contains(secret));
        assert!(file.after_image.as_ref().unwrap().contains("retained"));
    }

    #[test]
    fn line_change_survives_journal_json_without_rewriting_older_results() {
        let older = serde_json::to_value(ToolResult::builtin("done")).unwrap();
        assert!(older.get("line_change").is_none());
        let new = ToolResult::builtin("done").with_line_change(LineChange {
            added: 3,
            removed: 1,
        });
        let reloaded: ToolResult =
            serde_json::from_value(serde_json::to_value(new).unwrap()).unwrap();
        assert_eq!(
            reloaded.line_change(),
            Some(LineChange {
                added: 3,
                removed: 1
            })
        );
        let reloaded_old: ToolResult = serde_json::from_value(older).unwrap();
        assert_eq!(reloaded_old.line_change(), None);
    }
}
