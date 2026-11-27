//! `PreparedCall`, `Permit` and the `GuardedIo` seam.
//!
//! See `ARCHITECTURE.md` §3.2 (enforced rather than declared effects) and
//! §3.4 (permissions and sandbox). Effects are governed by a permit minted
//! only by the policy engine, never by a tool's own declaration.

mod adaptive;
pub mod analysis;
mod approver;
mod authority;
mod dev_commands;
mod execution_policy;
pub mod fake;
mod foreign;
mod fs;
mod grant;
pub mod judge;
mod owner_administration;
mod path;
mod prepared_call;
pub mod residual;
mod roots;
pub mod rule;
mod sandbox_grant;
mod scan;
mod scope;
mod session_approvals;
pub use session_approvals::SessionApprovals;
pub mod shell;
mod spawn;
mod tool;
mod write_floor;

pub use adaptive::{AdaptivePerimeter, ReadPerimeter};
/// `ModeApprover` is deliberately not re-exported: §6 step 5's implementation
/// is chosen by `PolicyEngine::new` from the resolved mode, and a type a
/// caller could name is one step from a caller that supplies one.
pub use approver::{Approver, HumanApprover, PermissionDecision, Verdict};
pub use authority::{Authority, RuleAuthority};
pub use execution_policy::ExecutionPolicy;
pub use foreign::{
    CleanupCertainty, DnsResolution, DuplexEnd, DuplexHandle, DuplexSession, DuplexSettlement,
    DuplexSpawner, ExecutableDenial, ForeignToolId, ForeignTransportTarget, ForeignValueError,
    GuardedHttp, HttpBody, HttpHeader, HttpRequest, HttpRequestDigest, HttpRoute, HttpScheme,
    InvocationDigest, NetworkTarget, OneHop, OneHopPayload, OneHopResponse, PreparedConnect,
    ResolvedConnect, Resolver, Subject, TransportError, TransportFailure, WebSocketSession,
};
pub use fs::{
    Fs, FsError, FsWriteAttempt, GuardedEffects, GuardedIo, GuardedWriteAttempt, IoDenied,
    MAX_WEB_REDIRECTS, McpAcpClaim, McpHttpClaim, McpStdioClaim, PathResolver,
};
pub use grant::{
    Authorized, Classification, Escalation, Pending, PermissionPresentation, PermissionRequest,
    Permit, PermitDenied, PolicyDenied, PolicyEngine, Resolution,
};
pub(crate) use grant::{InheritedGrant, PermissionPresentationInput};
pub use path::{CanonPath, has_stream_spelling, names_alternate_stream};
pub use prepared_call::{
    Change, HookHttpPreparedEnvelope, McpPreparedEnvelope, NotebookCellEdit, NotebookCellType, Op,
    OpName, PreparedCall, ReplaceChunk, SecretEgressAuthority, SessionProcessAction,
};
pub use roots::{
    DerivedRoot, PermanentRootConsent, ProfileRoot, RootAccess, RootCeilings, RootCfg, RootConsent,
    RootsError, admit_roots, check_root_ceilings, expand_root_path, resolve_root,
};
pub use sandbox_grant::{RefusalAnswer, SandboxAccess, SandboxGrant, SandboxGrantPort};
pub use scan::{Entry, EntryKind, Found, LineMatcher, Match, Scan, ScanLimits, ScanRequest};
pub use scope::Scope;
pub use spawn::{
    BackendIdentity, Confinement, ConfinementParseError, DetachedExecLines, DetachedExecMonitor,
    DetachedExecOutcome, DetachedExecSnapshot, EnvName, EnvNameError, EnvPolicy, ExecCaps,
    ExecObservation, ExecRequirements, ExplicitEnvBinding, ExplicitEnvValueError, MissingExecCaps,
    MonitoredLine, PolicyDigest, PolicyDigestError, ProcessError, ProcessExecutor, RunConfinement,
    RunConfinementError, SessionOutputObservation, SpawnSpec, SpawnSpecError,
};
pub(crate) use tool::ToolResultProvenance;
pub use tool::{
    BEFORE_IMAGE_LIMIT, BeforeImage, FileWriteEvidence, LineChange,
    MAX_TOOL_ERROR_DIAGNOSTIC_BYTES, PrepareCtx, StreamProvenance, Tool, ToolContentBlock,
    ToolDescriptor, ToolError, ToolOutputEncoding, ToolOutputProvenance, ToolResult, Typed,
    TypedTool,
};
pub use write_floor::GIT_CONTROLS;

mod terminal_text;
pub use terminal_text::{TerminalSafe, shell_word, terminal_output_safe, terminal_safe};

#[cfg(test)]
mod session_approval_tests;
