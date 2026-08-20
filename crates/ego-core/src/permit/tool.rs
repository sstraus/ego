//! `Tool`, the dynamic-dispatch trait every tool implements, and `Typed<T>`,
//! the adapter builtin tools use to get a `schemars`-derived JSON Schema and
//! typed params for free (`ARCHITECTURE.md` §3.2).

use futures::future::BoxFuture;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::fs::{GuardedIo, PathResolver};
use super::grant::{Permit, PermitDenied};
use super::path::CanonPath;
use super::prepared_call::PreparedCall;

/// A tool's identity and parameter schema, as shown to the model.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub schema: schemars::Schema,
}

/// What `Tool::prepare` sees: enough to resolve canonical operations, never
/// enough to perform any (`ARCHITECTURE.md` §3.2 — `prepare` runs before a
/// `Permit` exists, so it gets a [`PathResolver`], not a [`GuardedIo`]).
pub struct PrepareCtx<'a> {
    pub cwd: CanonPath,
    pub resolver: &'a dyn PathResolver,
}

/// Why a `Tool` call failed, before or during execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// `prepare`'s params didn't match the tool's schema.
    InvalidParams(String),
    /// `execute` hit an operation the `Permit` doesn't cover.
    Denied(PermitDenied),
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ToolError::InvalidParams(msg) => write!(f, "invalid params: {msg}"),
            ToolError::Denied(denied) => write!(f, "{denied}"),
        }
    }
}

impl std::error::Error for ToolError {}

impl From<PermitDenied> for ToolError {
    fn from(denied: PermitDenied) -> Self {
        ToolError::Denied(denied)
    }
}

/// A tool's output.
///
/// `ARCHITECTURE.md` §3.2 calls out `ToolResult` as evidence-plus-provenance,
/// not a bare string — that full shape (content blocks, provenance, a
/// stored full-output reference with `read_tool_result` truncation) is a
/// later story; M0a needs just enough to prove `execute` ran under a
/// checked `Permit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub content: String,
}

/// One tool, dynamically dispatched (`ARCHITECTURE.md` §3.2): MCP tools are
/// dynamic by nature, and boxing one trait beats a static registry macro
/// plus a second registration mechanism for builtins. `execute` receives a
/// `Permit` and a `GuardedIo` — never the raw `Fs` — so an effect can only
/// happen through a checked seam.
pub trait Tool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    fn prepare(&self, params: Value, ctx: &PrepareCtx<'_>) -> Result<PreparedCall, ToolError>;

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        permit: &'a Permit,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>;
}

/// The typed half of a builtin tool: params with a `schemars`-derived
/// schema instead of hand-written JSON Schema and manual `Value` parsing.
pub trait TypedTool: Send + Sync {
    type Params: JsonSchema + DeserializeOwned;

    fn name(&self) -> &str;
    fn description(&self) -> &str;

    fn prepare_typed(
        &self,
        params: Self::Params,
        ctx: &PrepareCtx<'_>,
    ) -> Result<PreparedCall, ToolError>;

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        permit: &'a Permit,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>>;
}

/// Adapts any [`TypedTool`] into a [`Tool`]: derives the JSON Schema via
/// `schemars`, and turns a parse failure into `ToolError::InvalidParams` —
/// model feedback, not a panic (`ARCHITECTURE.md` §3.2).
pub struct Typed<T>(pub T);

impl<T: TypedTool> Tool for Typed<T> {
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

    fn execute<'a>(
        &'a self,
        call: PreparedCall,
        permit: &'a Permit,
        io: &'a dyn GuardedIo,
    ) -> BoxFuture<'a, Result<ToolResult, ToolError>> {
        self.0.execute(call, permit, io)
    }
}
