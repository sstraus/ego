//! `PreparedCall`, `Permit` and the `GuardedIo` seam.
//!
//! See `ARCHITECTURE.md` §3.2 (enforced rather than declared effects) and
//! §3.4 (permissions and sandbox). Effects are governed by a permit minted
//! only by the policy engine, never by a tool's own declaration.

mod approver;
mod authority;
pub mod fake;
mod foreign;
mod fs;
mod grant;
pub mod judge;
mod path;
mod prepared_call;
pub mod rule;
mod scan;
mod scope;
mod spawn;
mod tool;

/// `ModeApprover` is deliberately not re-exported: §6 step 5's implementation
/// is chosen by `PolicyEngine::new` from the resolved mode, and a type a
/// caller could name is one step from a caller that supplies one.
pub use approver::{Approver, HumanApprover, PermissionDecision, Verdict};
pub use authority::{Authority, RuleAuthority};
pub use foreign::{
    CleanupCertainty, DnsResolution, DuplexEnd, DuplexHandle, DuplexSession, DuplexSettlement,
    DuplexSpawner, ForeignToolId, ForeignTransportTarget, ForeignValueError, GuardedHttp, HttpBody,
    HttpHeader, HttpRequest, HttpRequestDigest, HttpRoute, HttpScheme, InvocationDigest,
    NetworkTarget, OneHop, OneHopResponse, PreparedConnect, ResolvedConnect, Resolver, Subject,
    TransportError, TransportFailure,
};
pub use fs::{Fs, FsError, GuardedEffects, GuardedIo, IoDenied, McpHttpClaim, PathResolver};
pub use grant::{
    Authorized, Classification, Escalation, Pending, PermissionRequest, Permit, PermitDenied,
    PolicyDenied, PolicyEngine, Resolution,
};
pub use path::CanonPath;
pub use prepared_call::{Change, McpPreparedEnvelope, Op, OpName, PreparedCall};
pub use scan::{Entry, EntryKind, Found, LineMatcher, Match, Scan, ScanLimits, ScanRequest};
pub use scope::Scope;
pub use spawn::{
    BackendIdentity, Confinement, ConfinementParseError, EnvName, EnvNameError, EnvPolicy,
    ExecCaps, ExecObservation, ExecRequirements, MissingExecCaps, PolicyDigest, PolicyDigestError,
    ProcessError, ProcessExecutor, RunConfinement, RunConfinementError, SpawnSpec, SpawnSpecError,
};
pub use tool::{
    MAX_TOOL_ERROR_DIAGNOSTIC_BYTES, PrepareCtx, StreamProvenance, Tool, ToolContentBlock,
    ToolDescriptor, ToolError, ToolOutputEncoding, ToolOutputProvenance, ToolResult, Typed,
    TypedTool,
};
