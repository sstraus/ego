//! `PreparedCall`, `Permit` and the `GuardedIo` seam.
//!
//! See `ARCHITECTURE.md` §3.2 (enforced rather than declared effects) and
//! §3.4 (permissions and sandbox). Effects are governed by a permit minted
//! only by the policy engine, never by a tool's own declaration.

mod fs;
mod grant;
mod path;
mod prepared_call;
mod scope;
mod tool;

pub use fs::{Fs, GuardedFs, GuardedIo, PathResolver};
pub use grant::{Permit, PermitDenied, PolicyEngine};
pub use path::CanonPath;
pub use prepared_call::{Op, PreparedCall};
pub use scope::Scope;
pub use tool::{PrepareCtx, Tool, ToolDescriptor, ToolError, ToolResult, Typed, TypedTool};
