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
use super::prepared_call::{Op, PreparedCall};
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
    List { path: CanonPath },
    Search { path: CanonPath },
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
            PermitDenied::List { path } => {
                write!(f, "list denied: {path} is outside the permit's scope")
            }
            PermitDenied::Search { path } => {
                write!(f, "search denied: {path} is outside the permit's scope")
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
    /// The one prefix check the four path authorities share.
    ///
    /// Written once rather than four times: the *policy* difference between
    /// reading and listing lives entirely in which list is consulted, and
    /// four copies of this loop would be four places for that decision to
    /// drift.
    fn allows(
        list: &[CanonPath],
        path: &CanonPath,
        deny: fn(CanonPath) -> PermitDenied,
    ) -> Result<(), PermitDenied> {
        if list.iter().any(|allowed| path.is_or_under(allowed)) {
            Ok(())
        } else {
            Err(deny(path.clone()))
        }
    }

    /// Checks a write against [`Scope::write_paths`]. Callers pass a
    /// freshly resolved [`CanonPath`] — never one cached from an earlier
    /// point in time — so this only ever judges the current truth.
    pub fn allows_write(&self, path: &CanonPath) -> Result<(), PermitDenied> {
        Self::allows(&self.scope.write_paths, path, |path| PermitDenied::Write {
            path,
        })
    }

    /// Checks a read against [`Scope::read_paths`].
    pub fn allows_read(&self, path: &CanonPath) -> Result<(), PermitDenied> {
        Self::allows(&self.scope.read_paths, path, |path| PermitDenied::Read {
            path,
        })
    }

    /// Checks an enumeration against [`Scope::list_paths`].
    ///
    /// Deliberately does not fall back to [`Scope::read_paths`]: a permit that
    /// may read a file says nothing about whether the directory holding it may
    /// be enumerated, and inferring one from the other is how least privilege
    /// quietly stops being enforced.
    pub fn allows_list(&self, path: &CanonPath) -> Result<(), PermitDenied> {
        Self::allows(&self.scope.list_paths, path, |path| PermitDenied::List {
            path,
        })
    }

    /// Checks a search against [`Scope::search_paths`].
    pub fn allows_search(&self, path: &CanonPath) -> Result<(), PermitDenied> {
        Self::allows(&self.scope.search_paths, path, |path| {
            PermitDenied::Search { path }
        })
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

/// Why policy refused to authorise a call. Names the operation, never just
/// the fact of a refusal — `ARCHITECTURE.md` §3.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDenied {
    /// The call wants to write, and this milestone's tools are read-only.
    WriteRefused { tool: String, path: CanonPath },
    /// The call wants to reach outside the workspace. Carries which kind of
    /// operation it was: "you may not read /etc/passwd" and "you may not list
    /// /etc" are different refusals, and a prompt that flattens them into
    /// "denied" teaches a model nothing.
    OutsideWorkspace {
        tool: String,
        op: &'static str,
        path: CanonPath,
    },
}

impl fmt::Display for PolicyDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyDenied::WriteRefused { tool, path } => write!(
                f,
                "{tool} may not write {path}: this session's tools are read-only"
            ),
            PolicyDenied::OutsideWorkspace { tool, op, path } => {
                write!(f, "{tool} may not {op} {path}: it is outside the workspace")
            }
        }
    }
}

impl std::error::Error for PolicyDenied {}

/// The sole minter of [`Permit`]s (`ARCHITECTURE.md` §3.2, §3.4).
///
/// Stands in for the full rule-grammar-plus-judge engine of §3.4 axes 2–3,
/// which is a later milestone; for M0b it answers one question — see
/// [`authorize`](Self::authorize). There is deliberately no `widen` method,
/// here or anywhere: a call that needs a broader grant is a new decision,
/// made by minting a new `Permit`, not by mutating an old one.
#[derive(Debug)]
pub struct PolicyEngine {
    /// Everything at or under this path may be read. Nothing else may.
    workspace: CanonPath,
}

impl PolicyEngine {
    /// A policy that admits reads inside `workspace`.
    pub fn new(workspace: CanonPath) -> Self {
        Self { workspace }
    }

    /// Decide what a prepared call may do, and hand back the grant that says
    /// so (`docs/decisions/2026-08-21-the-turn-loop.md` §1).
    ///
    /// This is the method that makes this an engine rather than a stamp. The
    /// scope is derived *from the call*, so a permit can never allow more
    /// than the call asked for — with [`mint`](Self::mint) alone, whoever
    /// chose the `Scope` was the real policy, and the caller is the turn
    /// loop, which has no business holding that authority.
    ///
    /// M0b's rule is one line long: a read inside the workspace is admitted,
    /// and nothing else is. `ARCHITECTURE.md` §3.4's grammar, modes and judge
    /// are M1's; the shape here is what lets them arrive without moving
    /// anything — a mode that asks a human still returns a `Permit` or a
    /// `PolicyDenied` from this method.
    pub fn authorize(&self, call: &PreparedCall) -> Result<Permit, PolicyDenied> {
        let mut scope = Scope::default();
        for op in &call.ops {
            if let Op::Write { path, .. } = op {
                return Err(PolicyDenied::WriteRefused {
                    tool: call.tool.clone(),
                    path: path.clone(),
                });
            }
            let path = op.path();
            if !path.is_or_under(&self.workspace) {
                return Err(PolicyDenied::OutsideWorkspace {
                    tool: call.tool.clone(),
                    op: op.name(),
                    path: path.clone(),
                });
            }
            // Each op widens only its own authority. A call that lists a
            // directory gets `list_paths` and no `read_paths`, so the tool
            // learns names and cannot then read what it found — the whole
            // reason these are separate lists.
            match op {
                Op::Read { .. } => scope.read_paths.push(path.clone()),
                Op::List { .. } => scope.list_paths.push(path.clone()),
                Op::Search { .. } => scope.search_paths.push(path.clone()),
                Op::Write { .. } => unreachable!("refused above"),
            }
        }
        // Scoped to the paths this call named, not to the workspace: a tool
        // that asked to read one file is not thereby allowed to read the
        // repository. `GuardedIo` re-resolves at use time, so a symlink
        // swapped in afterwards is judged against this same narrow list.
        Ok(self.mint(scope))
    }

    /// Mint a permit for an explicit scope.
    ///
    /// The sealed primitive [`authorize`](Self::authorize) is built from, and
    /// what a later bypass mode (`--yolo`, `ARCHITECTURE.md` §3.4) mints a
    /// deliberately wide grant with. The turn loop does not call it: a loop
    /// that chooses its own `Scope` is a loop that is its own policy.
    pub fn mint(&self, scope: Scope) -> Permit {
        Permit {
            scope,
            _sealed: Sealed(()),
        }
    }
}
