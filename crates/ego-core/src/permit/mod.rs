//! `PreparedCall`, `Permit` and the `GuardedIo` seam.
//!
//! See `ARCHITECTURE.md` §3.2 (enforced rather than declared effects) and
//! §3.4 (permissions and sandbox). Effects are governed by a permit minted
//! only by the policy engine, never by a tool's own declaration.

pub mod analysis;
mod approver;
mod authority;
pub mod fake;
mod foreign;
mod fs;
mod grant;
pub mod judge;
mod orchestration;
mod path;
mod prepared_call;
mod roots;
pub mod rule;
mod sandbox_grant;
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
    NetworkTarget, OneHop, OneHopPayload, OneHopResponse, PreparedConnect, ResolvedConnect,
    Resolver, Subject, TransportError, TransportFailure,
};
pub use fs::{
    Fs, FsError, GuardedEffects, GuardedIo, IoDenied, McpAcpClaim, McpHttpClaim, McpStdioClaim,
    PathResolver,
};
pub(crate) use grant::PermissionPresentationInput;
pub use grant::{
    Authorized, Classification, Escalation, Pending, PermissionPresentation, PermissionRequest,
    Permit, PermitDenied, PolicyDenied, PolicyEngine, Resolution,
};
pub use path::CanonPath;
pub use prepared_call::{
    Change, HookHttpPreparedEnvelope, McpPreparedEnvelope, Op, OpName, PreparedCall,
    SecretEgressAuthority, SessionProcessAction,
};
pub use roots::{ProfileRoot, RootAccess, RootCfg, RootsError, admit_roots, check_root_ceilings};
pub use sandbox_grant::{SandboxAccess, SandboxGrant, SandboxGrantPort};
pub use scan::{Entry, EntryKind, Found, LineMatcher, Match, Scan, ScanLimits, ScanRequest};
pub use scope::Scope;
pub use spawn::{
    BackendIdentity, Confinement, ConfinementParseError, DetachedExecOutcome, DetachedExecSnapshot,
    EnvName, EnvNameError, EnvPolicy, ExecCaps, ExecObservation, ExecRequirements,
    ExplicitEnvBinding, ExplicitEnvValueError, MissingExecCaps, PolicyDigest, PolicyDigestError,
    ProcessError, ProcessExecutor, RunConfinement, RunConfinementError, SpawnSpec, SpawnSpecError,
};
pub(crate) use tool::ToolResultProvenance;
pub use tool::{
    MAX_TOOL_ERROR_DIAGNOSTIC_BYTES, PrepareCtx, StreamProvenance, Tool, ToolContentBlock,
    ToolDescriptor, ToolError, ToolOutputEncoding, ToolOutputProvenance, ToolResult, Typed,
    TypedTool,
};
