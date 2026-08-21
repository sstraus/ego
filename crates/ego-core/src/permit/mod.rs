//! `PreparedCall`, `Permit` and the `GuardedIo` seam.
//!
//! See `ARCHITECTURE.md` §3.2 (enforced rather than declared effects) and
//! §3.4 (permissions and sandbox). Effects are governed by a permit minted
//! only by the policy engine, never by a tool's own declaration.

pub mod fake;
mod fs;
mod grant;
mod path;
mod prepared_call;
pub mod rule;
mod scan;
mod scope;
mod tool;

pub use fs::{Fs, FsError, GuardedFs, GuardedIo, IoDenied, PathResolver};
pub use grant::{Authorized, Permit, PermitDenied, PolicyDenied, PolicyEngine};
pub use path::CanonPath;
pub use prepared_call::{Op, PreparedCall};
pub use scan::{Entry, EntryKind, Found, LineMatcher, Match, Scan, ScanLimits, ScanRequest};
pub use scope::Scope;
pub use tool::{PrepareCtx, Tool, ToolDescriptor, ToolError, ToolResult, Typed, TypedTool};
