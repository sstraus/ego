//! `Permit`: an unforgeable, scoped grant, minted only by [`PolicyEngine`].
//!
//! See `ARCHITECTURE.md` §3.2 (R1: effects enforced, not declared) and
//! §3.4. A tool never constructs or widens its own `Permit`: both of
//! `Permit`'s fields are private to this module, and its only public
//! constructor lives behind [`Sealed`], whose own constructor is private to
//! this module too. No code outside `grant.rs` — not `tool.rs`, not
//! `fs.rs`, not a consumer crate — can name `Sealed(())`, so the only way
//! to produce a `Permit` anywhere in the program is this module's private
//! `mint`, and the only way to reach *that* is
//! [`PolicyEngine::authorize`], which derives the scope from the call.

use std::fmt;

use super::path::CanonPath;
use super::prepared_call::{Op, PreparedCall};
use super::scope::Scope;

/// A capability token whose constructor only this module can reach. Its
/// only job is to make [`Permit`]'s constructor equally unreachable from
/// anywhere else — see the module doc comment.
#[derive(Debug)]
struct Sealed(());

/// An unforgeable, scoped grant produced only by the policy engine.
///
/// Deliberately does not implement `Clone`: every `Permit` that exists is
/// one the policy engine chose to mint, and there is no operation — here or
/// anywhere else — that turns one `Permit` into a second, possibly wider,
/// one. A call that needs more must go back to the engine for a fresh
/// decision.
///
/// # Leg 1 of Invariant W: unforgeability, decided by the compiler
///
/// `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` names three legs
/// of proof and says only the first is total. This is the first.
///
/// These run as doctests, which rustdoc compiles as an *external* crate
/// against the public API. That is the right vantage point: the question is
/// what someone outside `ego-core` can construct, not what this file can.
///
/// **Every `compile_fail` block below is paired with a companion block that
/// must compile, and the two differ only in the forbidden line.** The pairing
/// is the whole instrument. A lone `compile_fail` passes when the code fails
/// to build *for any reason at all* — a typo, a renamed import, a moved
/// module — so on its own it proves nothing and degrades silently the first
/// time something is renamed around it. The companion is the control: if the
/// setup stops compiling, the companion goes red and says so.
///
/// The error codes named in prose are what each block *does* fail with,
/// recorded to say which rule is doing the work. They are documentation and
/// not assertions: rustdoc accepts a `compile_fail,E0451` annotation on
/// stable and does not check it — verified by changing one to a nonexistent
/// `E0001`, which still passed. The annotations are therefore left off rather
/// than written in a form that reads like a check and is not one.
///
/// **A permit cannot be duplicated.** `E0277`: the bound is not satisfied,
/// because `Clone` is not implemented. Written as a bound rather than as
/// `permit.clone()`, which would silently compile — `&Permit` is `Clone`
/// however `Permit` is declared, and would hand back another reference.
///
/// ```compile_fail
/// fn assert_clone<T: Clone>() {}
/// assert_clone::<ego_core::permit::Permit>();
/// ```
///
/// Control — the same helper shape, the same path, a bound `Permit` does
/// satisfy:
///
/// ```
/// fn assert_debug<T: std::fmt::Debug>() {}
/// assert_debug::<ego_core::permit::Permit>();
/// ```
///
/// **A permit cannot be built by naming its fields.** `E0451`: the fields
/// are private.
///
/// ```compile_fail
/// use ego_core::permit::Permit;
/// let _forged = Permit { scope: Default::default(), _sealed: Default::default() };
/// ```
///
/// Control — the type is nameable and the import resolves; only the literal
/// is refused:
///
/// ```
/// use ego_core::permit::Permit;
/// fn takes(_: &Permit) {}
/// ```
///
/// **The seal cannot be named.** `E0432`: `Sealed` is not exported, so the
/// import does not resolve. It is the whole reason `Permit`'s constructor is
/// unreachable rather than merely inconvenient.
///
/// ```compile_fail
/// use ego_core::permit::Sealed;
/// ```
///
/// Control — `ego_core::permit` is the right module and it does export
/// things, so the failure above is `Sealed`'s absence and not a bad path:
///
/// ```
/// use ego_core::permit::Permit;
/// fn takes(_: &Permit) {}
/// ```
///
/// **A permit's scope cannot be read out and re-minted.** `E0616`: the field
/// is private. There is no accessor either, and none returning `&mut`.
///
/// ```compile_fail
/// use ego_core::permit::{CanonPath, Op, PolicyEngine, PreparedCall};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"));
/// let granted = engine
///     .authorize(PreparedCall {
///         tool: "read".to_string(),
///         ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
///     })
///     .unwrap();
/// let _scope = &granted.permit().scope;
/// ```
///
/// Control — every other line of that setup compiles; only the field access
/// does not:
///
/// ```
/// use ego_core::permit::{CanonPath, Op, PolicyEngine, PreparedCall};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"));
/// let granted = engine
///     .authorize(PreparedCall {
///         tool: "read".to_string(),
///         ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
///     })
///     .unwrap();
/// let _permit = granted.permit();
/// ```
///
/// **What this leg does not prove.** Nothing here says the scope is
/// *enforced*. That is `GuardedIo`'s job and `POTENTIALS.md` P23's open
/// window: a permit that cannot be widened, checked against a path that is
/// re-resolved between the check and the open, is still a hole. Leg 1 is
/// about where permits come from, and about nothing else.
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

/// A call and the grant that answers it, as one value.
///
/// [`Invariant W`](PolicyEngine) says a permit belongs to a call. Until this
/// type existed nothing in the types said so: `authorize` handed back a bare
/// [`Permit`], and a caller holding the permit from call A alongside call B
/// had two values it could pass in either combination. Pairing them here
/// makes the correspondence structural — the same argument
/// `2026-08-21-attempt-accounting-ledger.md` used to keep a reservation
/// inside its `GenerationAttempt` rather than beside it.
///
/// No `Clone` and no public constructor: an `Authorized` is produced by
/// [`PolicyEngine::authorize`] and by nothing else.
///
/// # Leg 1, continued
///
/// **An `Authorized` cannot be assembled from a call and someone else's
/// permit.** `E0451`: both fields are private, so there is no literal to
/// write. This is the mutation §3 exists to kill — hold the permit from call
/// A, pair it with call B, execute.
///
/// ```compile_fail
/// use ego_core::permit::{Authorized, CanonPath, Op, PolicyEngine, PreparedCall};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"));
/// let a = engine
///     .authorize(PreparedCall {
///         tool: "read".to_string(),
///         ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
///     })
///     .unwrap();
/// let b = PreparedCall {
///     tool: "read".to_string(),
///     ops: vec![Op::Read { path: CanonPath::new("/ws/b.rs") }],
/// };
/// let _widened = Authorized { call: b, permit: a.permit() };
/// ```
///
/// Control — both halves exist and are reachable; what is refused is putting
/// them together:
///
/// ```
/// use ego_core::permit::{CanonPath, Op, PolicyEngine, PreparedCall};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"));
/// let a = engine
///     .authorize(PreparedCall {
///         tool: "read".to_string(),
///         ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
///     })
///     .unwrap();
/// let b = PreparedCall {
///     tool: "read".to_string(),
///     ops: vec![Op::Read { path: CanonPath::new("/ws/b.rs") }],
/// };
/// let (_, _) = (a.permit(), b);
/// ```
///
/// **And it cannot be split into its halves.** `E0624`: `into_parts` is
/// crate-private. Splitting is what would make a pair available to mismatch
/// in the first place, and `turn/tools.rs` is the only caller.
///
/// ```compile_fail
/// use ego_core::permit::{CanonPath, Op, PolicyEngine, PreparedCall};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"));
/// let granted = engine
///     .authorize(PreparedCall {
///         tool: "read".to_string(),
///         ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
///     })
///     .unwrap();
/// let (_call, _permit) = granted.into_parts();
/// ```
///
/// Control — the same `granted`, interrogated through what *is* public:
///
/// ```
/// use ego_core::permit::{CanonPath, Op, PolicyEngine, PreparedCall};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"));
/// let granted = engine
///     .authorize(PreparedCall {
///         tool: "read".to_string(),
///         ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
///     })
///     .unwrap();
/// let (_call, _permit) = (granted.call(), granted.permit());
/// ```
#[derive(Debug)]
pub struct Authorized {
    call: PreparedCall,
    permit: Permit,
}

impl Authorized {
    /// The call this grant answers.
    pub fn call(&self) -> &PreparedCall {
        &self.call
    }

    /// The grant, borrowed.
    ///
    /// A borrow is not a widening: it cannot be cloned, cannot be minted
    /// from, and cannot outlive the `Authorized` that owns it. What it is
    /// for is [`GuardedFs`](super::GuardedFs), which binds one permit to one
    /// `Fs` at construction and rechecks every path against it at use time.
    pub fn permit(&self) -> &Permit {
        &self.permit
    }

    /// Split the pair, immediately before execution.
    ///
    /// `pub(crate)` and not `pub`: `turn/tools.rs` is the one place that
    /// needs the call by value and the permit by reference at the same
    /// instant, and it re-pairs them in the same expression. Outside this
    /// crate there is no way to obtain a free-standing `Permit` at all, so
    /// there is nothing to pair with the wrong call.
    pub(crate) fn into_parts(self) -> (PreparedCall, Permit) {
        (self.call, self.permit)
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
/// # Invariant W
///
/// > For every `Permit` p that exists at any instant of the process, there is
/// > exactly one `PreparedCall` c such that every authority in p's scope was
/// > contributed by an `Op` of c, and p was minted during the authorization
/// > of c. No operation anywhere produces a second `Permit` from a first, and
/// > no operation mutates a live one.
///
/// # Leg 1, continued
///
/// **`mint` is unreachable.** `E0599`: there is no such method on
/// `PolicyEngine` from outside `grant.rs`. This is the widening primitive —
/// give it a `Scope` and it hands back the matching authority, with no call
/// involved — and its absence from the public surface is what makes
/// [`authorize`](Self::authorize) the only source of a permit.
///
/// ```compile_fail
/// use ego_core::permit::{CanonPath, PolicyEngine, Scope};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"));
/// let _wide = engine.mint(Scope::default());
/// ```
///
/// Control — the engine is constructible and `Scope::default()` exists; the
/// method is the only missing piece:
///
/// ```
/// use ego_core::permit::{CanonPath, PolicyEngine, Scope};
/// let _engine = PolicyEngine::new(CanonPath::new("/ws"));
/// let _empty = Scope::default();
/// ```
///
/// **And a `Scope` cannot be composed to feed it.** `E0451`: the fields are
/// `pub(super)`. Even reachable, `mint` would have nothing to be given.
///
/// ```compile_fail
/// use ego_core::permit::{CanonPath, Scope};
/// let _reachable = Scope {
///     read_paths: vec![CanonPath::new("/")],
///     ..Default::default()
/// };
/// ```
///
/// Control — a `Scope` is still a public, readable value. Composing one is
/// what is refused, not naming or reading one:
///
/// ```
/// use ego_core::permit::Scope;
/// let empty = Scope::default();
/// assert!(empty.read_paths().is_empty());
/// ```
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
    /// anything — a mode that asks a human still returns an [`Authorized`] or
    /// a `PolicyDenied` from this method.
    ///
    /// Takes the call **by value** and gives it back inside the
    /// [`Authorized`], so the pairing is the only thing a caller can hold.
    pub fn authorize(&self, call: PreparedCall) -> Result<Authorized, PolicyDenied> {
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
        let permit = self.mint(scope);
        Ok(Authorized { call, permit })
    }

    /// Mint a permit for an explicit scope.
    ///
    /// **Private to `grant.rs`, and that is the load-bearing half of
    /// Invariant W.** It used to be `pub`, reserved for "a later bypass mode
    /// (`--yolo`) minting a deliberately wide grant". §1 of
    /// `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` removed that
    /// use: bypass is a decision *whether to ask*, and a bypassed call is
    /// still scoped from the call, so nothing anywhere needs a wide grant.
    /// What was left was a public widening primitive kept for its own sake —
    /// any holder of a `&PolicyEngine` could compose a `Scope` covering the
    /// whole disk and get a `Permit` for it.
    ///
    /// With it private, [`authorize`](Self::authorize) is the only path to a
    /// `Permit` in the entire program, and `authorize` derives the scope from
    /// the call. A loop that chooses its own `Scope` is a loop that is its own
    /// policy, and now it cannot.
    fn mint(&self, scope: Scope) -> Permit {
        Permit {
            scope,
            _sealed: Sealed(()),
        }
    }
}

/// Scope mechanics, and the one place `mint` can still be called.
///
/// These tests live inside `grant.rs` rather than under `tests/` because
/// they need [`PolicyEngine::mint`], and `mint` is private to this module —
/// that privacy is half of §2 of
/// `docs/decisions/2026-08-21-a-permit-nobody-can-widen.md`, and a test
/// binary that could reach it would be proof it is not private at all. What
/// they check is what a `Scope` *means* once minted: the four path lists do
/// not imply one another, `is_or_under` is component-wise and not a string
/// prefix, and the guarded seam rechecks at use time.
#[cfg(test)]
mod tests {
    use futures::executor::block_on;
    use serde_json::json;

    use super::*;
    use crate::permit::fake::{FakeFs, FakeWriteTool};
    use crate::permit::{
        Fs, GuardedFs, GuardedIo, IoDenied, Op, PrepareCtx, Tool, ToolError, Typed,
    };

    fn prepare_ctx<'a>(fs: &'a FakeFs) -> PrepareCtx<'a> {
        PrepareCtx {
            cwd: CanonPath::new("/ws"),
            resolver: fs,
        }
    }

    #[test]
    fn permit_typed_tool_writes_inside_permit_ok() {
        let fs = FakeFs::default();
        let tool = Typed(FakeWriteTool);
        let ctx = prepare_ctx(&fs);
        let call = tool
            .prepare(
                json!({ "path": "/ws/allowed/note.txt", "contents": "hello" }),
                &ctx,
            )
            .unwrap();

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        let result = block_on(tool.execute(call, &permit, &guarded)).unwrap();
        assert_eq!(result.content, "wrote 5 bytes to /ws/allowed/note.txt");
        assert_eq!(
            fs.read(&CanonPath::new("/ws/allowed/note.txt")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn permit_typed_tool_write_outside_permit_denied_names_op() {
        let fs = FakeFs::default();
        let tool = Typed(FakeWriteTool);
        let ctx = prepare_ctx(&fs);
        let call = tool
            .prepare(json!({ "path": "/etc/passwd", "contents": "pwned" }), &ctx)
            .unwrap();

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        let err = block_on(tool.execute(call, &permit, &guarded)).unwrap_err();
        assert_eq!(
            err,
            ToolError::Denied(PermitDenied::Write {
                path: CanonPath::new("/etc/passwd")
            })
        );
        let message = err.to_string();
        assert!(
            message.contains("write"),
            "denial should name the op: {message}"
        );
        assert!(
            message.contains("/etc/passwd"),
            "denial should name the path: {message}"
        );
        assert!(
            fs.read(&CanonPath::new("/etc/passwd")).is_err(),
            "denied write must not happen"
        );
    }

    #[test]
    fn permit_symlink_resolved_at_prepare_is_rechecked_at_execute() {
        let fs = FakeFs::default();
        fs.symlink("/ws/link", "/ws/allowed/file.txt");

        let tool = Typed(FakeWriteTool);
        let ctx = prepare_ctx(&fs);
        let call = tool
            .prepare(json!({ "path": "/ws/link", "contents": "hi" }), &ctx)
            .expect("prepare chases the symlink");
        assert_eq!(
            call.ops,
            vec![Op::Write {
                path: CanonPath::new("/ws/allowed/file.txt"),
                contents: b"hi".to_vec(),
            }]
        );

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });

        // Race: between prepare (which classified /ws/allowed/file.txt as
        // the target) and execute, the resolved target itself is swapped
        // for a symlink pointing outside the permitted scope.
        fs.symlink("/ws/allowed/file.txt", "/ws/secret/passwords.txt");

        let guarded = GuardedFs::new(&permit, &fs);
        let err = block_on(tool.execute(call, &permit, &guarded)).unwrap_err();
        assert_eq!(
            err,
            ToolError::Denied(PermitDenied::Write {
                path: CanonPath::new("/ws/secret/passwords.txt"),
            })
        );
        assert!(
            fs.read(&CanonPath::new("/ws/secret/passwords.txt"))
                .is_err(),
            "the raced-to target must never be written"
        );
    }

    #[test]
    fn permit_guarded_fs_read_respects_scope() {
        let fs = FakeFs::default();
        fs.file("/ws/readable/notes.md", "public notes");
        fs.file("/ws/private/notes.md", "private notes");

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            read_paths: vec![CanonPath::new("/ws/readable")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        assert_eq!(
            guarded
                .read(&CanonPath::new("/ws/readable/notes.md"))
                .unwrap(),
            b"public notes".to_vec()
        );
        assert!(matches!(
            guarded.read(&CanonPath::new("/ws/private/notes.md")),
            Err(IoDenied::Permit(PermitDenied::Read { .. }))
        ));
    }

    #[test]
    fn permit_guarded_fs_write_denies_sibling_prefix_directory() {
        let fs = FakeFs::default();
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        // "/ws/allowed-evil" shares a textual prefix with the scoped
        // "/ws/allowed" but is a sibling, not a descendant of it —
        // `Path::starts_with` is component-wise, so this must stay denied.
        // A string-prefix "simplification" of `CanonPath::is_or_under`
        // would silently let this through.
        let err = guarded
            .write(&CanonPath::new("/ws/allowed-evil/file.txt"), b"pwned")
            .unwrap_err();
        assert!(matches!(err, IoDenied::Permit(PermitDenied::Write { .. })));
        assert!(
            fs.read(&CanonPath::new("/ws/allowed-evil/file.txt"))
                .is_err(),
            "denied write must not happen"
        );
    }

    #[test]
    fn permit_guarded_fs_write_allows_scope_root_and_nested_descendant() {
        let fs = FakeFs::default();
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        // The scope boundary itself is in scope ("is or under", not
        // strictly-under-only)...
        guarded
            .write(&CanonPath::new("/ws/allowed"), b"root")
            .unwrap();
        // ...and so is a nested descendant, arbitrarily deep.
        guarded
            .write(&CanonPath::new("/ws/allowed/nested/deep/file.txt"), b"deep")
            .unwrap();

        assert_eq!(fs.read(&CanonPath::new("/ws/allowed")).unwrap(), b"root");
        assert_eq!(
            fs.read(&CanonPath::new("/ws/allowed/nested/deep/file.txt"))
                .unwrap(),
            b"deep"
        );
    }

    #[test]
    fn permit_guarded_fs_read_denies_sibling_prefix_directory() {
        let fs = FakeFs::default();
        fs.file("/ws/allowed-evil/secret.txt", "leaked");

        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            read_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(&permit, &fs);

        // Same sibling-prefix hazard as the write case, checked against
        // `Scope::read_paths` — read and write scope independently, each
        // with its own `.iter().any()`, so this needs its own pin.
        let err = guarded
            .read(&CanonPath::new("/ws/allowed-evil/secret.txt"))
            .unwrap_err();
        assert!(matches!(err, IoDenied::Permit(PermitDenied::Read { .. })));
    }

    #[test]
    fn permit_scope_covers_paths_endpoints_and_argv() {
        let engine = PolicyEngine::new(CanonPath::new("/ws"));
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            read_paths: vec![CanonPath::new("/ws/readable")],
            list_paths: vec![CanonPath::new("/ws/listable")],
            search_paths: vec![CanonPath::new("/ws/searchable")],
            endpoints: vec!["api.example.com:443".to_string()],
            argv: vec![vec!["git".to_string(), "status".to_string()]],
        });

        assert!(
            permit
                .allows_write(&CanonPath::new("/ws/allowed/file.txt"))
                .is_ok()
        );
        assert!(matches!(
            permit.allows_write(&CanonPath::new("/etc/passwd")),
            Err(PermitDenied::Write { .. })
        ));
        // Write scope does not imply read scope, or vice versa.
        assert!(matches!(
            permit.allows_read(&CanonPath::new("/ws/allowed/file.txt")),
            Err(PermitDenied::Read { .. })
        ));

        assert!(
            permit
                .allows_read(&CanonPath::new("/ws/readable/notes.md"))
                .is_ok()
        );
        assert!(matches!(
            permit.allows_read(&CanonPath::new("/ws/private/notes.md")),
            Err(PermitDenied::Read { .. })
        ));

        assert!(permit.allows_connect("api.example.com:443").is_ok());
        assert!(matches!(
            permit.allows_connect("evil.example.com:443"),
            Err(PermitDenied::Connect { .. })
        ));

        assert!(
            permit
                .allows_argv(&[
                    "git".to_string(),
                    "status".to_string(),
                    "--short".to_string()
                ])
                .is_ok()
        );
        assert!(matches!(
            permit.allows_argv(&["rm".to_string(), "-rf".to_string()]),
            Err(PermitDenied::Argv { .. })
        ));
    }
}
