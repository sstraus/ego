//! `Permit`: an unforgeable, scoped grant, minted only by [`PolicyEngine`].
//!
//! See `ARCHITECTURE.md` §3.2 (R1: effects enforced, not declared) and
//! §3.4. A tool never constructs or widens its own `Permit`: both of
//! `Permit`'s fields are private to this module, and its only public
//! constructor lives behind [`Sealed`], whose own constructor is private to
//! this module too. No code outside `grant.rs` — not `tool.rs`, not
//! `fs.rs`, not a consumer crate — can name `Sealed(())`, so the only way
//! to produce a `Permit` anywhere in the program is [`PolicyEngine::mint`].

use std::fmt;

use super::path::CanonPath;
use super::scope::Scope;

/// A capability token whose constructor only this module can reach. Its
/// only job is to make [`Permit`]'s constructor equally unreachable from
/// anywhere else — see the module doc comment.
#[derive(Debug)]
struct Sealed(());

/// An unforgeable, scoped grant produced only by [`PolicyEngine::mint`].
///
/// Deliberately does not implement `Clone`: every `Permit` that exists is
/// one the policy engine chose to mint, and there is no operation — here or
/// anywhere else — that turns one `Permit` into a second, possibly wider,
/// one. A call that needs more must go back to the engine for a fresh
/// decision.
#[derive(Debug)]
pub struct Permit {
    scope: Scope,
    _sealed: Sealed,
}

/// Names the operation a [`Permit`] refused. `ARCHITECTURE.md` §3.2: a
/// denial must name what was denied, not just that something was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermitDenied {
    Write { path: CanonPath },
    Read { path: CanonPath },
    Connect { endpoint: String },
    Argv { argv: Vec<String> },
}

impl fmt::Display for PermitDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PermitDenied::Write { path } => {
                write!(f, "write denied: {path} is outside the permit's scope")
            }
            PermitDenied::Read { path } => {
                write!(f, "read denied: {path} is outside the permit's scope")
            }
            PermitDenied::Connect { endpoint } => {
                write!(
                    f,
                    "connect denied: {endpoint} is outside the permit's scope"
                )
            }
            PermitDenied::Argv { argv } => {
                write!(
                    f,
                    "spawn denied: argv {argv:?} is outside the permit's scope"
                )
            }
        }
    }
}

impl std::error::Error for PermitDenied {}

impl Permit {
    /// Checks a write against [`Scope::write_paths`]. Callers pass a
    /// freshly resolved [`CanonPath`] — never one cached from an earlier
    /// point in time — so this only ever judges the current truth.
    pub fn allows_write(&self, path: &CanonPath) -> Result<(), PermitDenied> {
        if self
            .scope
            .write_paths
            .iter()
            .any(|allowed| path.is_or_under(allowed))
        {
            Ok(())
        } else {
            Err(PermitDenied::Write { path: path.clone() })
        }
    }

    /// Checks a read against [`Scope::read_paths`].
    pub fn allows_read(&self, path: &CanonPath) -> Result<(), PermitDenied> {
        if self
            .scope
            .read_paths
            .iter()
            .any(|allowed| path.is_or_under(allowed))
        {
            Ok(())
        } else {
            Err(PermitDenied::Read { path: path.clone() })
        }
    }

    /// Checks a network endpoint against [`Scope::endpoints`].
    pub fn allows_connect(&self, endpoint: &str) -> Result<(), PermitDenied> {
        if self
            .scope
            .endpoints
            .iter()
            .any(|allowed| allowed == endpoint)
        {
            Ok(())
        } else {
            Err(PermitDenied::Connect {
                endpoint: endpoint.to_string(),
            })
        }
    }

    /// Checks a spawned command's argv against [`Scope::argv`]'s patterns.
    pub fn allows_argv(&self, argv: &[String]) -> Result<(), PermitDenied> {
        if self
            .scope
            .argv
            .iter()
            .any(|pattern| argv.starts_with(pattern))
        {
            Ok(())
        } else {
            Err(PermitDenied::Argv {
                argv: argv.to_vec(),
            })
        }
    }
}

/// The sole minter of [`Permit`]s (`ARCHITECTURE.md` §3.2, §3.4).
///
/// Stands in for the full rule-grammar-plus-judge engine of §3.4 axes 2–3,
/// which is a later milestone; for M0a its entire job is to be the one
/// place that can name `Sealed(())`. There is deliberately no `widen`
/// method, here or anywhere: a call that needs a broader grant is a new
/// decision, made by minting a new `Permit`, not by mutating an old one.
#[derive(Debug, Default)]
pub struct PolicyEngine;

impl PolicyEngine {
    pub fn mint(&self, scope: Scope) -> Permit {
        Permit {
            scope,
            _sealed: Sealed(()),
        }
    }
}
