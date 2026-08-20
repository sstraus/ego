//! `PreparedCall`: canonical operations, resolved at `prepare` time — never
//! prose, never a rendered shell string (`ARCHITECTURE.md` §3.2, §3.4).
//! This is what the policy engine classifies to mint a `Permit`, and what a
//! judge model evaluates in `auto` mode.

use super::path::CanonPath;

/// One canonical operation a `Tool::execute` intends to perform.
///
/// M0a's [`GuardedIo`](super::GuardedIo) covers only the fs subset here;
/// `Connect`/`Spawn` variants belong to the process/network confinement
/// stories that build on this one (`ARCHITECTURE.md` §3.4 axis 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Read { path: CanonPath },
    Write { path: CanonPath, contents: Vec<u8> },
}

impl Op {
    /// The violating-operation name a denial or an approval prompt shows.
    pub fn name(&self) -> &'static str {
        match self {
            Op::Read { .. } => "read",
            Op::Write { .. } => "write",
        }
    }
}

/// A tool call reduced to its canonical operations by `Tool::prepare`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCall {
    pub tool: String,
    pub ops: Vec<Op>,
}
