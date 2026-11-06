//! Human-authorized widening of the process sandbox.
//!
//! A confined child that is refused a path outside the workspace is not a
//! policy decision: the sandbox profile is fixed for the run, and the person
//! on the other side of the host is the only party who can say the path may be
//! reached. This module is the vocabulary between the process executor that
//! sees the refusal and the host seat that can answer it. It carries no
//! authority itself; the executor applies a grant to the profile of its own
//! children and to nothing else.

use std::fmt;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use super::CanonPath;

/// What a confined child tried to do to a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxAccess {
    Read,
    Write,
    Exec,
}

impl fmt::Display for SandboxAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Exec => "exec",
        })
    }
}

/// One canonical path and the access a human allowed (or is asked to allow)
/// on it. A grant never covers a parent directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SandboxGrant {
    pub path: CanonPath,
    pub access: SandboxAccess,
}

impl SandboxGrant {
    pub fn new(path: CanonPath, access: SandboxAccess) -> Self {
        Self { path, access }
    }

    /// Whether this grant already permits `access` on `path`. A write or exec
    /// grant includes read, because the profile derives both from a readable
    /// root.
    pub fn covers(&self, path: &CanonPath, access: SandboxAccess) -> bool {
        path.is_or_under(&self.path)
            && (self.access == access
                || (access == SandboxAccess::Read
                    && matches!(self.access, SandboxAccess::Write | SandboxAccess::Exec)))
    }
}

impl fmt::Display for SandboxGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.access, self.path)
    }
}

/// The host seat that answers sandbox denials for one run.
pub trait SandboxGrantPort: Send + Sync {
    /// Every grant currently in force for this run: the session's answers and
    /// whatever the user stored for the workspace.
    fn granted(&self) -> Vec<SandboxGrant>;

    /// Ask the human whether `request` may be added to the sandbox. `true`
    /// means the port has recorded the grant, and the next [`Self::granted`]
    /// includes it; anything else, including a failed transport, is `false`.
    /// Show `command`, the exact refused command. An approval never replays it.
    fn ask<'a>(&'a self, request: &'a SandboxGrant, command: &'a str) -> BoxFuture<'a, bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(path: &str, access: SandboxAccess) -> SandboxGrant {
        SandboxGrant::new(CanonPath::new(path), access)
    }

    #[test]
    fn a_write_grant_covers_read_below_it_but_a_read_grant_never_covers_write() {
        let write = grant("/data/out", SandboxAccess::Write);
        assert!(write.covers(&CanonPath::new("/data/out/a.txt"), SandboxAccess::Read));
        assert!(!write.covers(&CanonPath::new("/data/outside"), SandboxAccess::Read));
        let read = grant("/data/in", SandboxAccess::Read);
        assert!(!read.covers(&CanonPath::new("/data/in/a.txt"), SandboxAccess::Write));
        assert!(!read.covers(&CanonPath::new("/data/in/a.txt"), SandboxAccess::Exec));
    }
}
