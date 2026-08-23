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
//! [`PolicyEngine::classify`], which derives the scope from the call.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::approver::{Approver, ModeApprover, Verdict};
use super::authority::{Authority, RuleAuthority};
use super::path::CanonPath;
use super::prepared_call::{Op, OpName, PreparedCall};
use super::rule::RuleId;
use super::scope::Scope;
use crate::config::{Mode, OriginSet, Ruleset, Source, SourcedRule};

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
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::rule::Rule;
/// use ego_core::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall};
/// let allow = vec![Rule::load("read(**)").expect("a rule this build enforces")];
/// let engine = PolicyEngine::new(
///     CanonPath::new("/ws"),
///     Mode::Default,
///     Ruleset::from_layer(PermCfg { allow, deny: Vec::new() }, Source::Project),
/// );
/// let call = PreparedCall {
///     tool: "read".to_string(),
///     ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
/// };
/// let Classification::Settled(Ok(granted)) = engine.classify(call)
///     else { panic!("an allow rule covers this read") };
/// let _scope = &granted.permit().scope;
/// ```
///
/// Control — every other line of that setup compiles; only the field access
/// does not:
///
/// ```
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::rule::Rule;
/// use ego_core::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall};
/// let allow = vec![Rule::load("read(**)").expect("a rule this build enforces")];
/// let engine = PolicyEngine::new(
///     CanonPath::new("/ws"),
///     Mode::Default,
///     Ruleset::from_layer(PermCfg { allow, deny: Vec::new() }, Source::Project),
/// );
/// let call = PreparedCall {
///     tool: "read".to_string(),
///     ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
/// };
/// let Classification::Settled(Ok(granted)) = engine.classify(call)
///     else { panic!("an allow rule covers this read") };
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
/// type existed nothing in the types said so: `classify` handed back a bare
/// [`Permit`], and a caller holding the permit from call A alongside call B
/// had two values it could pass in either combination. Pairing them here
/// makes the correspondence structural — the same argument
/// `2026-08-21-attempt-accounting-ledger.md` used to keep a reservation
/// inside its `GenerationAttempt` rather than beside it.
///
/// No `Clone` and no public constructor: an `Authorized` is produced by
/// [`PolicyEngine::classify`] and by nothing else.
///
/// # Leg 1, continued
///
/// **An `Authorized` cannot be taken apart by naming its fields.** `E0616`:
/// both are private. Reaching the permit as a *field* is what would let it be
/// moved out and re-paired with another call — §3's whole subject.
///
/// ```compile_fail
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::rule::Rule;
/// use ego_core::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall};
/// let allow = vec![Rule::load("read(**)").expect("a rule this build enforces")];
/// let engine = PolicyEngine::new(
///     CanonPath::new("/ws"),
///     Mode::Default,
///     Ruleset::from_layer(PermCfg { allow, deny: Vec::new() }, Source::Project),
/// );
/// let call = PreparedCall {
///     tool: "read".to_string(),
///     ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
/// };
/// let Classification::Settled(Ok(granted)) = engine.classify(call)
///     else { panic!("an allow rule covers this read") };
/// let _stolen = &granted.permit;
/// ```
///
/// Control — the same value through the accessor, which lends and cannot be
/// moved out:
///
/// ```
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::rule::Rule;
/// use ego_core::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall};
/// let allow = vec![Rule::load("read(**)").expect("a rule this build enforces")];
/// let engine = PolicyEngine::new(
///     CanonPath::new("/ws"),
///     Mode::Default,
///     Ruleset::from_layer(PermCfg { allow, deny: Vec::new() }, Source::Project),
/// );
/// let call = PreparedCall {
///     tool: "read".to_string(),
///     ops: vec![Op::Read { path: CanonPath::new("/ws/a.rs") }],
/// };
/// let Classification::Settled(Ok(granted)) = engine.classify(call)
///     else { panic!("an allow rule covers this read") };
/// let _borrowed = granted.permit();
/// ```
///
/// **Assembling one is refused by field privacy, and by nothing else.**
/// `E0451`. This is worth stating precisely, because an earlier draft of this
/// comment claimed two independent reasons — field privacy, *and* that no
/// owned `Permit` could be obtained outside `ego-core` at all. The second is
/// no longer true. `into_parts` is public, so the ingredients for the forgery
/// are freely available; only the constructor refuses. The block below mints
/// them on purpose rather than reaching for a borrow, so that it fails on the
/// privacy it names.
///
/// ```compile_fail
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::rule::Rule;
/// use ego_core::permit::{Authorized, CanonPath, Classification, Op, PolicyEngine, PreparedCall};
/// let allow = vec![Rule::load("read(**)").expect("a rule this build enforces")];
/// let engine = PolicyEngine::new(
///     CanonPath::new("/ws"),
///     Mode::Default,
///     Ruleset::from_layer(PermCfg { allow, deny: Vec::new() }, Source::Project),
/// );
/// let split = |path: &str| {
///     let call = PreparedCall {
///         tool: "read".to_string(),
///         ops: vec![Op::Read { path: CanonPath::new(path) }],
///     };
///     let Classification::Settled(Ok(granted)) = engine.classify(call)
///         else { panic!("an allow rule covers this read") };
///     granted.into_parts()
/// };
/// let (_, permit) = split("/ws/a.rs");
/// let (call, _) = split("/ws/b.rs");
/// let _forged = Authorized { call, permit };
/// ```
///
/// Control — the same two splits, without the literal. It compiles, which is
/// the half that matters: the ingredients really are in hand, and the forgery
/// above fails on the constructor rather than on a missing value.
///
/// ```
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::rule::Rule;
/// use ego_core::permit::{CanonPath, Classification, Op, PolicyEngine, PreparedCall};
/// let allow = vec![Rule::load("read(**)").expect("a rule this build enforces")];
/// let engine = PolicyEngine::new(
///     CanonPath::new("/ws"),
///     Mode::Default,
///     Ruleset::from_layer(PermCfg { allow, deny: Vec::new() }, Source::Project),
/// );
/// let split = |path: &str| {
///     let call = PreparedCall {
///         tool: "read".to_string(),
///         ops: vec![Op::Read { path: CanonPath::new(path) }],
///     };
///     let Classification::Settled(Ok(granted)) = engine.classify(call)
///         else { panic!("an allow rule covers this read") };
///     granted.into_parts()
/// };
/// let (_, _permit) = split("/ws/a.rs");
/// let (_call, _) = split("/ws/b.rs");
/// ```
///
/// **The forgery buys nothing even where it is available.** Inside
/// `ego-core`, where the literal *does* compile, pairing call `b` with permit
/// `a` hands the tool `b`'s operations under `a`'s authority — and `a`'s
/// authority is the ceiling. Every path `b` names that `a` did not is refused
/// by [`GuardedFs`](super::GuardedFs) at use time. Mispairing can lose
/// authority; it cannot gain any. Invariant W is about what `mint` was told,
/// and `into_parts` does not mint.
///
/// **The earlier draft of the first block was found wrong by running the
/// mutation, not by reading it.** It wrote the literal
/// `Authorized { call: b, permit: a.permit() }` and passed — on a *type*
/// error, because `permit()` lends and the field wants an owned value. Making
/// both fields `pub` did not disturb it, so it asserted nothing whatever
/// about privacy. The `compile_fail`-plus-control pairing does not catch
/// that; only mutating the modifier does. Recorded here because the same trap
/// is waiting for anyone who writes the next one.
#[derive(Debug)]
pub struct Authorized {
    call: PreparedCall,
    permit: Permit,
    by: Authority,
}

impl Authorized {
    /// The call this grant answers.
    pub fn call(&self) -> &PreparedCall {
        &self.call
    }

    /// What let this call through, for the audit event.
    pub fn by(&self) -> &Authority {
        &self.by
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
    /// Public, and that gives nothing away. This **consumes** a validly built
    /// pair; it does not construct one. Outside `ego-core` the halves cannot
    /// be put back together at all — the fields are private and there is no
    /// constructor — and inside it, re-pairing them wrongly loses authority
    /// rather than gaining it, because the permit is the ceiling. See the type
    /// comment.
    ///
    /// The one production caller is `turn/tools.rs`, which splits and
    /// immediately re-pairs: the permit goes into the seam, the call goes to
    /// the tool, and the tool is handed no permit at all.
    pub fn into_parts(self) -> (PreparedCall, Permit) {
        (self.call, self.permit)
    }
}

/// Why policy refused to authorise a call. Names the operation, never just
/// the fact of a refusal — `ARCHITECTURE.md` §3.2.
///
/// Every arm is a **settled** answer: the question was asked and the answer is
/// no. A call nobody in this run can answer is not here — it is
/// [`Classification::Ask`], which is a different thing wearing a different
/// type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDenied {
    /// A deny rule covers this operation. Carries the rule's ID, because
    /// "denied" without it leaves an operator grepping their config for which
    /// of forty lines did it — and the ID is exactly what `ego rules revoke`
    /// takes.
    ///
    /// It also carries **where the rule came from**, because the ID alone does
    /// not answer the question an operator actually has. `RuleId` hashes the
    /// predicate, so the same rule written in two layers has one ID by
    /// construction — its own doc says the origin "is metadata that `rules ls`
    /// shows" — and a run that ends at exit 3 has no second invocation in
    /// which to show it. See
    /// `docs/decisions/2026-08-21-a-deny-that-names-who-can-lift-it.md`.
    Denied {
        tool: String,
        op: OpName,
        path: CanonPath,
        by: RuleId,
        /// The layer whose copy cannot be got around: the answer to "can I
        /// change this". Decided by [`SourcedRule::attribution`] and never
        /// recomputed here.
        from: Source,
        /// Every other layer that also denies this predicate. Empty for an
        /// ordinary single-layer rule.
        also: OriginSet,
    },
    /// The mode forbids this operation outright — `plan` and a mutating op.
    /// Not a rule and not relaxable by one: the floor is evaluated before
    /// every allow.
    ModeForbids {
        tool: String,
        mode: Mode,
        op: OpName,
        path: CanonPath,
    },
    /// The call wants to reach outside the workspace. Carries which kind of
    /// operation it was: "you may not read /etc/passwd" and "you may not list
    /// /etc" are different refusals, and a prompt that flattens them into
    /// "denied" teaches a model nothing.
    OutsideWorkspace {
        tool: String,
        op: OpName,
        path: CanonPath,
    },
    /// A party that was asked said no. Distinct from `Denied` because nobody
    /// wrote a rule: there is nothing to grep for and nothing to revoke, and
    /// telling an operator to look for a rule that does not exist is worse
    /// than saying less.
    ///
    /// `by` names *who* refused. §5 deferred this field to "the audit event
    /// that needs it"; that event has arrived, so the field has.
    Refused {
        tool: String,
        op: OpName,
        path: CanonPath,
        by: Authority,
    },
}

impl PolicyDenied {
    /// Who refused, for `EventKind::PermissionDecided`.
    ///
    /// One authority in every arm, and that is the shape of a refusal rather
    /// than a simplification: `judge` stops at the first step that says no, so
    /// exactly one thing did. The list in [`Authority::Rules`] is a list for
    /// the *allow* case, where nothing stops early.
    pub fn authority(&self) -> Authority {
        match self {
            PolicyDenied::Denied { by, from, also, .. } => Authority::Rules {
                rules: vec![RuleAuthority {
                    id: by.clone(),
                    from: *from,
                    also: *also,
                }],
            },
            PolicyDenied::ModeForbids { mode, .. } => Authority::Mode { mode: *mode },
            PolicyDenied::OutsideWorkspace { .. } => Authority::Boundary,
            PolicyDenied::Refused { by, .. } => by.clone(),
        }
    }
}

impl fmt::Display for PolicyDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PolicyDenied::Denied {
                tool,
                op,
                path,
                by,
                from,
                also,
            } => {
                write!(f, "{tool} may not {op} {path}: denied by {by} in the ")?;
                // Decision 3: the remedy is *in* the sentence, not inferable
                // from it. A system deny is the fact an operator most needs
                // and least expects — every other layer is theirs to edit, so
                // the default assumption is that this one is too.
                match from {
                    Source::System => write!(
                        f,
                        "system config, which no --allow and no higher layer can lift"
                    )?,
                    _ => write!(f, "{from} config")?,
                }
                // Decision 5a: a subordinate clause, and only when there is
                // something to subordinate. Every layer here lost to `from`,
                // which is what makes the advice true of all of them.
                if !also.is_empty() {
                    let list = also
                        .iter()
                        .map(|source| source.to_string())
                        .collect::<Vec<_>>()
                        .join(" and ");
                    // Agreement, because `also` is a set and a sentence that
                    // reads "the user and project config denies it" would be
                    // wrong the first time a second layer appears. Today
                    // `resolve` produces at most one — the merge loop leaves
                    // one layer's copy and only the system pass unions — so
                    // this is the renderer being correct about its input type
                    // rather than about a planned feature.
                    let (noun, verb) = match also.len() {
                        1 => ("config", "denies"),
                        _ => ("configs", "deny"),
                    };
                    write!(
                        f,
                        "; the {list} {noun} {verb} it too, so removing that changes nothing"
                    )?;
                }
                Ok(())
            }
            PolicyDenied::ModeForbids {
                tool,
                mode,
                op,
                path,
            } => write!(
                f,
                "{tool} may not {op} {path}: {mode} mode forbids every \
                 operation that changes anything"
            ),
            PolicyDenied::OutsideWorkspace { tool, op, path } => {
                write!(f, "{tool} may not {op} {path}: it is outside the workspace")
            }
            PolicyDenied::Refused { tool, op, path, .. } => {
                write!(f, "{tool} may not {op} {path}: permission was refused")
            }
        }
    }
}

impl std::error::Error for PolicyDenied {}

/// Why a call could not be settled without asking somebody.
///
/// A value and not a `bool` because `ARCHITECTURE.md` §3.4 requires the
/// *reason* to reach both the human and the audit event: they produce
/// different prompts and different headless behaviours.
///
/// **Two arms, not four.** `2026-08-21-a-permit-nobody-can-widen.md` §4 also
/// names "the judge was unavailable" and "the judge was uncertain". There is
/// no judge in this build, so those have no producer and are not declared —
/// the same rule that keeps `Op::Spawn` out of the `Op` enum.
///
/// The two that *are* declared are the two steps that can decline: the rules,
/// and then the mode. They are chained rather than alternative — a question
/// the rules did not cover reaches the mode carrying `NoRuleCovers`, and the
/// mode replaces it with its own reason if it declines too. Both are true; the
/// second is the one that stopped the run, and it is the one an operator can
/// act on.
///
/// Serialisable, because it is recorded in `EventKind::PermissionRequested`.
/// That is why `op` is an [`OpName`] and not the `&'static str` it began as —
/// see `OpName`'s own doc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Escalation {
    /// No allow rule covered it. `PolicyEngine::classify` is the producer:
    /// steps 0 to 3 ran and none of them settled it.
    NoRuleCovers { op: OpName, path: CanonPath },
    /// The mode does not auto-approve this operation. `ModeApprover` is the
    /// producer, at §6 step 5.
    ModeAsks {
        mode: Mode,
        op: OpName,
        path: CanonPath,
    },
}

impl Escalation {
    /// The operation the escalation is about. Every arm names one, because a
    /// question that cannot say what it is about is not a question anybody
    /// can answer.
    pub fn op(&self) -> OpName {
        match self {
            Escalation::NoRuleCovers { op, .. } | Escalation::ModeAsks { op, .. } => *op,
        }
    }

    pub fn path(&self) -> &CanonPath {
        match self {
            Escalation::NoRuleCovers { path, .. } | Escalation::ModeAsks { path, .. } => path,
        }
    }
}

impl fmt::Display for Escalation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Escalation::NoRuleCovers { op, path } => {
                write!(f, "no rule covers {op} {path}")
            }
            Escalation::ModeAsks { mode, op, path } => {
                write!(f, "{mode} mode does not approve {op} {path} on its own")
            }
        }
    }
}

/// What a host would render to ask a human.
///
/// **It does not carry the [`Scope`].** The scope is ego's internal answer to
/// a question the human has not answered yet, and a host that could read one
/// is one step from a host that proposes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    tool: String,
    ops: Vec<Op>,
    why: Escalation,
}

impl PermissionRequest {
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// The canonical operations, which is what a prompt must show. Rendering
    /// the tool's raw parameters instead would show the model's words rather
    /// than the resolved effect — the thing `prepare` exists to replace.
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    pub fn why(&self) -> &Escalation {
        &self.why
    }
}

impl fmt::Display for PermissionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} needs permission: {}", self.tool, self.why)
    }
}

/// A call that reached the end of the evaluation order without being settled.
///
/// No `Clone`, private fields, and constructible only in this module. A loop
/// can carry one and hand it back; it cannot open one, and it cannot turn one
/// into a [`Permit`] — [`PolicyEngine::classify`] remains the only path to
/// one, so §5's separation is enforced by visibility rather than by a comment.
///
/// **It does not carry the computed `Scope`, and §5 says it should.** With no
/// approver in this build there is no `grant`, so a carried scope would be a
/// field nothing reads. The scope is instead derived by one private function
/// that `classify` calls and that `grant` will call, which keeps the single
/// definition §5 wanted.
///
/// That is not a smaller version of §5's property. It is a larger one, and the
/// difference is what each rests on: **a private field is hidden from the
/// loop; an unreachable function cannot be named by it.** Hiding is a fact
/// about visibility, which one `pub(crate)` erases. Unreachability is a fact
/// about the call graph. The field version would also need a `dead_code`
/// allow until `grant` exists, and an `allow` outlives the reason somebody
/// wrote it.
#[derive(Debug)]
pub struct Pending {
    call: PreparedCall,
    ask: PermissionRequest,
}

impl Pending {
    /// The call that could not be settled.
    pub fn call(&self) -> &PreparedCall {
        &self.call
    }

    /// What to put to whoever can answer.
    pub fn request(&self) -> &PermissionRequest {
        &self.ask
    }
}

/// What [`PolicyEngine::classify`] concluded.
///
/// Two arms because "the answer is no" and "nobody here can answer" are
/// different facts. Both end a headless run at exit 3 today
/// (`CLI.md` §4.4), and they say different things while doing it — which is
/// the whole of what an operator has to go on.
#[derive(Debug)]
pub enum Classification {
    /// Decided, one way or the other, without asking anybody.
    Settled(Result<Authorized, PolicyDenied>),
    /// Not ours to answer.
    Ask(Pending),
}

/// What [`PolicyEngine::settle`] concluded after consulting a party.
///
/// `Ask` again rather than a third "unanswerable" arm: the question is not
/// finished, it is only finished *with this party*. §6 orders three of them
/// and the engine — never the loop — decides who is next, because the engine
/// is the only thing that may construct a [`Pending`] at all. With one party
/// in this build, "next" is nobody, and a loop that gets `Ask` back from
/// `settle` has exhausted the order: `CLI.md` §4.4, exit 3.
///
/// §5's `Reclassify` arm is not declared. It is the human's `Amend`, there is
/// no human, and an arm nothing produces reads as a working feature.
#[derive(Debug)]
pub enum Resolution {
    /// Decided. The party allowed it, or refused it.
    Settled(Result<Authorized, PolicyDenied>),
    /// Still open, with the reason this party gave for not answering.
    Ask(Pending),
}

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
/// [`classify`](Self::classify) the only source of a permit.
///
/// ```compile_fail
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::{CanonPath, PolicyEngine, Scope};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());
/// let _wide = engine.mint(Scope::default());
/// ```
///
/// Control — the engine is constructible and `Scope::default()` exists; the
/// method is the only missing piece:
///
/// ```
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::{CanonPath, PolicyEngine, Scope};
/// let _engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());
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
/// Two of §3.4's three axes are here: the mode and the rules. The third, the
/// judge, has nothing to build it out of yet, so a call the first two cannot
/// settle leaves as a question — see [`classify`](Self::classify).
///
/// # Leg 1, continued: nobody can supply the mode
///
/// Since §6 step 5 became the [`Approver`] port, "the mode decides" is only
/// true if the caller cannot decide which mode-shaped thing is consulted.
/// A port moves the choice of implementation somewhere, and wherever that
/// somewhere is, *it* becomes the mode. The same three blocks as above,
/// pointed at that:
///
/// **The mode's approver cannot be named.** `E0603`: `approver` is a private
/// module and `ModeApprover` is not re-exported, so there is no path to the
/// type a caller would have to mention in order to build a different one.
///
/// ```compile_fail
/// use ego_core::permit::approver::ModeApprover;
/// ```
///
/// Control — `Approver` and `Verdict` *are* exported, from the module a
/// caller really does reach, so the failure above is the type's absence and
/// not a mistyped path:
///
/// ```
/// use ego_core::permit::{Approver, Authority, Verdict};
/// fn takes(_: &dyn Approver) {}
/// let _answers = [Verdict::Allow { by: Authority::Party }, Verdict::Deny { by: Authority::Party }];
/// ```
///
/// **And there is no way to hand one in.** `E0599`: `with_approver` exists
/// only under `#[cfg(test)]`, inside this crate, so no release build contains
/// it and no other crate can reach it in any build. `new` takes a `Mode` and
/// builds the approver itself; a caller supplies the *configuration*, never
/// the party.
///
/// ```compile_fail
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::{CanonPath, PolicyEngine};
/// let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());
/// let _yolo = engine.with_approver(todo!());
/// ```
///
/// Control — the engine really is constructible from outside, and the mode
/// really is the argument it takes, so the block above fails on the method
/// and not on the setup:
///
/// ```
/// use ego_core::config::{Mode, PermCfg, Ruleset, Source};
/// use ego_core::permit::{CanonPath, PolicyEngine};
/// let _engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Yolo, Ruleset::default());
/// ```
///
/// **What this does not prove, and it is the same limit as everywhere else in
/// Leg 1.** Nothing here stops a caller passing `Mode::Yolo` to `new`. It is
/// not supposed to: choosing the mode is exactly what the config layers are
/// for, and `run.rs` passes `effective.mode`. The property is narrower and it
/// is the one that was at risk — *given* a resolved mode, no code downstream
/// of `new` can obtain a different mode's behaviour. Where the mode comes
/// from is `config::resolve`'s claim and `tests/config.rs` holds it.
///
/// There is deliberately no `widen` method, here or anywhere: a call that
/// needs a broader grant is a new decision, made by minting a new `Permit`,
/// not by mutating an old one.
#[derive(Debug)]
pub struct PolicyEngine {
    /// The boundary ego draws for itself. Not a rule: nobody wrote it, and no
    /// rule lifts it.
    workspace: CanonPath,
    mode: Mode,
    /// Both lists intact, matched per call. `resolve` no longer subtracts one
    /// from the other — that comparison was string equality between two lists
    /// and `deny = ["exec(git *)"]` left `allow = ["exec(git push)"]` standing.
    allow: Vec<SourcedRule>,
    deny: Vec<SourcedRule>,
    /// §6 step 5, built from `mode` by [`new`](Self::new) and reachable no
    /// other way. See [`ModeApprover`]'s doc for why there is no constructor
    /// that takes one: a port that let its holder pick the implementation
    /// would have moved the mode out of the config and into the caller.
    approver: Box<dyn Approver>,
}

impl PolicyEngine {
    /// A policy that decides `mode`'s way, under `permissions`, inside
    /// `workspace`.
    pub fn new(workspace: CanonPath, mode: Mode, permissions: Ruleset) -> Self {
        Self {
            workspace,
            mode,
            allow: permissions.allow,
            deny: permissions.deny,
            approver: Box::new(ModeApprover::new(mode)),
        }
    }

    /// The same engine with a different party at §6 step 5, for tests that
    /// need a verdict the mode cannot produce.
    ///
    /// **`#[cfg(test)]`, and only `ego-core`'s own test profile compiles it.**
    /// It exists because `Verdict::Deny` has no production producer — the
    /// mode never denies at step 5, since `plan`'s refusal is the floor at
    /// step 2 — and an arm of `settle` that nothing could reach would be
    /// untested code that reads as a decided behaviour. This is the seam that
    /// makes it a decided behaviour.
    ///
    /// It is not a relaxation of the rule above it. No release build contains
    /// this function, no other crate can call it, and the one caller is a
    /// test in this file asserting what a refusal turns into.
    #[cfg(test)]
    fn with_approver(self, approver: Box<dyn Approver>) -> Self {
        Self { approver, ..self }
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
    /// Takes the call **by value** and gives it back inside the
    /// [`Authorized`], so the pairing is the only thing a caller can hold.
    ///
    /// **Pure and sync**, which is what lets the async decision be somebody
    /// else's problem: nothing here awaits, opens anything or reads a clock.
    ///
    /// The order is fixed
    /// (`docs/decisions/2026-08-21-a-permit-nobody-can-widen.md` §6), first
    /// match wins, and it is evaluated **per operation** — a call with three
    /// ops is three questions and every one of them must be answered before
    /// the call is authorised. That is why §4 of the rule ADR refuses a
    /// conjunction across heads: it would be a second way to write what the
    /// loop below already does.
    ///
    /// ```text
    /// 0. the workspace boundary   (not a rule; no mode lifts it)
    /// 1. deny                     (system denies are unioned in by resolve)
    /// 2. mode floor               (plan refuses every mutating op)
    /// 3. allow
    /// →  ask
    /// ```
    ///
    /// Steps 1 and 2 of the ADR's seven are one step here, and nothing is lost
    /// by it: `resolve` unions the system layer's denies into the deny list
    /// through a ceiling no higher layer can clear, and deny precedes allow
    /// either way. What a separate step would buy is *attribution* in the
    /// audit event, and that arrives with the event.
    ///
    /// **Step 5 — the mode ceiling — is not here, and it used to be.** It is
    /// the first implementation of the [`Approver`] port, consulted by
    /// [`settle`](Self::settle). The reason is in the ADR's §5 amendment and
    /// it is not tidiness: with the ceiling inside this method, a `Pending`
    /// could only ever be constructed *after* the mode had declined, so an
    /// approver derived from the mode had exactly one reachable answer. The
    /// port existed and the seat had no occupant.
    ///
    /// Steps 6 and 7 — the judge and the human — still have no producer, so
    /// the order ends by returning the question rather than answering it.
    /// **Two passes, and the order between them is the point.** Every
    /// operation is put through steps 0 to 2 before any of them is allowed to
    /// raise a question, because "first match wins, top down" is a claim about
    /// the *steps*, not about the order the ops happen to be in. A single pass
    /// returns a question about op one and never reaches op two — so a call
    /// that reads a workspace file and `/etc/shadow` would arrive at an
    /// approver as an ordinary question, and a mode that answers yes would
    /// authorise the whole call, boundary included. A refusal anywhere refuses
    /// the call.
    pub fn classify(&self, call: PreparedCall) -> Classification {
        for op in &call.ops {
            if let Err(denied) = self.judge(&call, op) {
                return Classification::Settled(Err(denied));
            }
        }
        // Every rule that covered an op, not the first one to match. An
        // allow is a conjunction — the call proceeds because *all* of its ops
        // were covered — so naming one of them would record a rule that did
        // not, on its own, authorise anything. A denial is the other shape and
        // gets one authority, because `judge` stops at the first deny.
        let mut rules: Vec<RuleAuthority> = Vec::new();
        for op in &call.ops {
            let Some(entry) = self.matching(&self.allow, op) else {
                let why = Escalation::NoRuleCovers {
                    op: op.name(),
                    path: op.path().clone(),
                };
                return Classification::Ask(self.pending(call, why));
            };
            let authority = RuleAuthority::of(entry);
            if !rules.contains(&authority) {
                rules.push(authority);
            }
        }
        let permit = self.mint(scope_of(&call));
        Classification::Settled(Ok(Authorized {
            call,
            permit,
            by: Authority::Rules { rules },
        }))
    }

    /// Put the question to the next party in §6's order and act on the answer.
    ///
    /// The one `await` in `permit`. What this module still owns, and does not
    /// delegate, is *which* party is consulted and what their answer means: a
    /// verdict is a party's opinion, and only this method turns one into a
    /// [`Permit`] — from a scope derived, as always, from the call.
    ///
    /// `Escalate` hands the question back rather than ending it, carrying the
    /// reason this party gave. In this build there is nobody after the mode,
    /// so a caller that receives [`Resolution::Ask`] has exhausted the order
    /// and `CLI.md` §4.4 applies. When the judge and the human arrive they are
    /// consulted here, and no caller changes.
    pub async fn settle(&self, pending: Pending) -> Resolution {
        match self.approver.approve(&pending).await {
            Verdict::Allow { by } => {
                let call = pending.call;
                let permit = self.mint(scope_of(&call));
                Resolution::Settled(Ok(Authorized { call, permit, by }))
            }
            Verdict::Deny { by } => Resolution::Settled(Err(PolicyDenied::Refused {
                tool: pending.call.tool,
                op: pending.ask.why.op(),
                path: pending.ask.why.path().clone(),
                by,
            })),
            Verdict::Escalate { why } => Resolution::Ask(self.pending(pending.call, why)),
        }
    }

    /// The question, addressed to whoever is next.
    ///
    /// Private, and the only constructor of a [`Pending`] anywhere: §5's
    /// property is that the engine decides who is asked, and a `Pending` a
    /// caller could build is a caller nominating its own approver.
    fn pending(&self, call: PreparedCall, why: Escalation) -> Pending {
        Pending {
            ask: PermissionRequest {
                tool: call.tool.clone(),
                ops: call.ops.clone(),
                why,
            },
            call,
        }
    }

    /// Steps 0 to 2: everything that can refuse an operation outright.
    fn judge(&self, call: &PreparedCall, op: &Op) -> Result<(), PolicyDenied> {
        let path = op.path();

        // 0. The workspace boundary. Evaluated before the rules because it is
        //    not one of them: no operator wrote it, `--allow` cannot widen it,
        //    and `yolo` does not lift it. `yolo` bypasses *asking*; the
        //    sandbox (`ARCHITECTURE.md` §3.4 axis 4) is the axis that widens
        //    or narrows where ego may reach, and it is a separate control.
        if !path.is_or_under(&self.workspace) {
            return Err(PolicyDenied::OutsideWorkspace {
                tool: call.tool.clone(),
                op: op.name(),
                path: path.clone(),
            });
        }

        // 1. Deny, before any allow at any scope.
        if let Some(entry) = self.matching(&self.deny, op) {
            let (from, also) = entry.attribution();
            return Err(PolicyDenied::Denied {
                tool: call.tool.clone(),
                op: op.name(),
                path: path.clone(),
                by: entry.rule().id(),
                from,
                also,
            });
        }

        // 2. The mode floor. Above the allow list, so an allow rule cannot
        //    relax it: `plan` means the run does not change anything, and a
        //    mode a rule could switch off would not mean that.
        if self.mode == Mode::Plan && op.mutates() {
            return Err(PolicyDenied::ModeForbids {
                tool: call.tool.clone(),
                mode: self.mode,
                op: op.name(),
                path: path.clone(),
            });
        }

        Ok(())
    }

    /// The first rule in `rules` that covers `op`, if any.
    ///
    /// First match wins, in the order the lists were resolved. Matching is
    /// against the canonical operation and never against rendered text.
    fn matching<'r>(&self, rules: &'r [SourcedRule], op: &Op) -> Option<&'r SourcedRule> {
        rules
            .iter()
            .find(|entry| entry.rule().matches(op, self.workspace.as_path()))
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
    /// With it private, [`classify`](Self::classify) is the only path to a
    /// `Permit` in the entire program, and `classify` derives the scope from
    /// the call. A loop that chooses its own `Scope` is a loop that is its own
    /// policy, and now it cannot.
    fn mint(&self, scope: Scope) -> Permit {
        Permit {
            scope,
            _sealed: Sealed(()),
        }
    }
}

/// The authority a call's own operations contribute — **the** definition,
/// called once and never inlined at a second site.
///
/// §1 of `2026-08-21-a-permit-nobody-can-widen.md`: the scope comes from the
/// call, so a permit can never allow more than the call asked for and a rule
/// that matched could not widen it if it wanted to. Nothing decides here; by
/// the time this runs, every operation has already been approved.
///
/// Scoped to the paths this call named, not to the workspace: a tool that
/// asked to read one file is not thereby allowed to read the repository.
/// `GuardedIo` re-resolves at use time, so a symlink swapped in afterwards is
/// judged against this same narrow list.
fn scope_of(call: &PreparedCall) -> Scope {
    let mut scope = Scope::default();
    for op in &call.ops {
        let path = op.path().clone();
        // Each op widens only its own authority. A call that lists a
        // directory gets `list_paths` and no `read_paths`, so the tool learns
        // names and cannot then read what it found — the whole reason these
        // are separate lists.
        match op {
            Op::Read { .. } => scope.read_paths.push(path),
            Op::List { .. } => scope.list_paths.push(path),
            Op::Search { .. } => scope.search_paths.push(path),
            Op::Write { .. } => scope.write_paths.push(path),
        }
    }
    scope
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
        Change, Fs, GuardedFs, GuardedIo, IoDenied, Op, PrepareCtx, Tool, ToolError, Typed,
    };

    fn prepare_ctx<'a>(fs: &'a FakeFs) -> PrepareCtx<'a> {
        PrepareCtx {
            cwd: CanonPath::new("/ws"),
            resolver: fs,
        }
    }

    /// An engine whose only job below is to be the thing that can `mint`.
    ///
    /// The mode and the rules decide *whether* a scope is minted, and none of
    /// these tests goes through `classify` — they hand `mint` a scope
    /// directly and ask what the resulting permit means. So the defaults are
    /// not a choice here; there is nothing for them to affect.
    fn minter() -> PolicyEngine {
        PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default())
    }

    /// The second implementation of [`Approver`], and the only producer of
    /// [`Verdict::Deny`] anywhere.
    ///
    /// The mode never denies at §6 step 5 — `plan`'s refusal is the floor at
    /// step 2, inside `classify` — so without a party that can say no, the
    /// `Deny` arm of [`PolicyEngine::settle`] would be code nothing could
    /// reach. `2026-08-21-a-permit-nobody-can-widen.md` §5's amendment records
    /// why the arm ships anyway: `Approver` is a public port, and one whose
    /// vocabulary cannot express a refusal is a port the human seat at step 7
    /// would have to break in order to use it.
    ///
    /// It answers without asking anybody, which is the property the trait had
    /// to keep: an approver is a *party*, not a prompt, and a party that could
    /// only ever relay a question to somewhere else would make "the mode
    /// approves" unexpressible.
    #[derive(Debug)]
    struct AlwaysDenies;

    impl Approver for AlwaysDenies {
        fn approve<'a>(&'a self, _pending: &'a Pending) -> futures::future::BoxFuture<'a, Verdict> {
            Box::pin(futures::future::ready(Verdict::Deny {
                by: Authority::Party,
            }))
        }
    }

    fn write_call(path: &str) -> PreparedCall {
        PreparedCall {
            tool: "edit".to_string(),
            ops: vec![Op::Write {
                path: CanonPath::new(path),
                change: Change::Whole(b"whatever".to_vec()),
            }],
        }
    }

    #[test]
    fn a_party_that_refuses_settles_the_call_without_naming_a_rule() {
        let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default())
            .with_approver(Box::new(AlwaysDenies));
        let Classification::Ask(pending) = engine.classify(write_call("/ws/src/main.rs")) else {
            panic!("no rule covers it, so steps 0 to 3 cannot settle it");
        };

        let Resolution::Settled(Err(denied)) = block_on(engine.settle(pending)) else {
            panic!("the party said no, and no is an answer");
        };
        // `Refused` and not `Denied`: nobody wrote a rule, so there is no ID
        // to grep for and nothing for `ego rules revoke` to take. Sending an
        // operator looking for a rule that does not exist is worse than
        // saying less.
        assert_eq!(
            denied,
            PolicyDenied::Refused {
                tool: "edit".to_string(),
                op: OpName::Write,
                path: CanonPath::new("/ws/src/main.rs"),
                by: Authority::Party,
            }
        );
    }

    /// A refusal ends the call. It does not fall through to the next party,
    /// and it does not mint anything.
    ///
    /// Worth its own assertion because the `Deny` arm and the `Escalate` arm
    /// both stop the run in a headless build, so a `settle` that treated a
    /// refusal as an escalation would look identical from the outside — same
    /// exit code, same absence of an effect — while meaning the opposite. One
    /// says nobody could answer; the other says somebody did.
    #[test]
    fn a_refusal_is_an_answer_and_not_another_question() {
        let engine = PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default())
            .with_approver(Box::new(AlwaysDenies));
        let Classification::Ask(pending) = engine.classify(write_call("/ws/src/main.rs")) else {
            panic!("no rule covers it");
        };

        assert!(
            matches!(
                block_on(engine.settle(pending)),
                Resolution::Settled(Err(_))
            ),
            "a party that answered must not leave the question open"
        );
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

        let engine = minter();
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(permit, &fs);

        let result = block_on(tool.execute(call, &guarded)).unwrap();
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

        let engine = minter();
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(permit, &fs);

        let err = block_on(tool.execute(call, &guarded)).unwrap_err();
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
                change: Change::Whole(b"hi".to_vec()),
            }]
        );

        let engine = minter();
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });

        // Race: between prepare (which classified /ws/allowed/file.txt as
        // the target) and execute, the resolved target itself is swapped
        // for a symlink pointing outside the permitted scope.
        fs.symlink("/ws/allowed/file.txt", "/ws/secret/passwords.txt");

        let guarded = GuardedFs::new(permit, &fs);
        let err = block_on(tool.execute(call, &guarded)).unwrap_err();
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

        let engine = minter();
        let permit = engine.mint(Scope {
            read_paths: vec![CanonPath::new("/ws/readable")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(permit, &fs);

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
        let engine = minter();
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(permit, &fs);

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
        let engine = minter();
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(permit, &fs);

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

        let engine = minter();
        let permit = engine.mint(Scope {
            read_paths: vec![CanonPath::new("/ws/allowed")],
            ..Scope::default()
        });
        let guarded = GuardedFs::new(permit, &fs);

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
        let engine = minter();
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
