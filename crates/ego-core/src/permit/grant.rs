//! `Permit`: an unforgeable, scoped grant, minted only by [`PolicyEngine`].
//!
//! A tool never constructs or widens its own `Permit`: both of `Permit`'s fields are
//! private to this module, and its only public constructor lives behind [`Sealed`], whose
//! own constructor is private to this module too. No code outside `grant.rs` — not
//! `tool.rs`, not `fs.rs`, not a consumer crate — can name `Sealed(())`, so the only way
//! to produce a `Permit` anywhere in the program is this module's private `mint`, and the
//! only way to reach *that* is [`PolicyEngine::classify`], which derives the scope from
//! the call.

use std::fmt::{self, Write as _};
use std::sync::Arc;

use super::TerminalSafe;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::approver::{Approver, HumanApprover, ModeApprover, PermissionDecision, Verdict};
use super::authority::{Authority, RuleAuthority};
use super::foreign::{
    ForeignToolId, ForeignTransportTarget, InvocationDigest, NetworkTarget, PreparedConnect,
    Subject, TransportError, TransportFailure,
};
use super::path::CanonPath;
use super::prepared_call::{Change, Op, OpName, PreparedCall, SessionProcessAction};
use super::roots::{ProfileRoot, RootAccess};
use super::rule::RuleId;
use super::scope::Scope;
use super::spawn::SpawnSpec;
use crate::config::rule_management::{RememberPlan, RuleManagement};
use crate::config::{ManagedRuleEffect, Mode, OriginSet, Ruleset, Source, SourcedRule};
use crate::event::id::{DetachedExecId, ItemId};
use crate::store::evidence::EvidenceRead;

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
/// The external compiler contract lives in `tests/permit_privacy.rs` and its
/// trybuild fixtures. They prove that callers cannot clone or construct
/// `Permit`, name `Sealed`, read its private scope, construct `Authorized` or
/// `Scope`, or call the widening method. The declarations here remain the
/// source of truth: `Permit` is not `Clone`, `Sealed` is not exported, and no
/// widening constructor is public.
///
/// This leg does not prove that the scope is enforced. Leg 1 is only about where permits
/// come from.
#[derive(Debug)]
pub struct Permit {
    scope: Scope,
    read_perimeter: super::ReadPerimeter,
    call_fingerprint: Option<[u8; 32]>,
    _sealed: Sealed,
}

/// Names the operation a [`Permit`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermitDenied {
    Write {
        path: CanonPath,
    },
    Read {
        path: CanonPath,
    },
    List {
        path: CanonPath,
    },
    Search {
        path: CanonPath,
    },
    Connect {
        endpoint: String,
    },
    Spawn {
        spec: Box<SpawnSpec>,
    },
    Evidence {
        request: EvidenceRead,
    },
    SessionProcess {
        id: DetachedExecId,
        action: SessionProcessAction,
    },
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
            PermitDenied::Spawn { spec } => {
                write!(
                    f,
                    "spawn denied: specification {spec:?} is outside the permit's scope"
                )
            }
            PermitDenied::Evidence { request } => {
                write!(
                    f,
                    "evidence read denied: {} at byte {} is outside the permit's scope",
                    request.reference, request.offset
                )
            }
            PermitDenied::SessionProcess { id, action } => {
                write!(
                    f,
                    "session process {id} {action:?} is outside the permit's scope"
                )
            }
        }
    }
}

impl std::error::Error for PermitDenied {}

impl Permit {
    pub(crate) fn allows_call(&self, call: &PreparedCall) -> Result<(), TransportError> {
        if self.call_fingerprint == Some(call.authority_fingerprint()) {
            Ok(())
        } else {
            Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))
        }
    }

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
        if self.read_perimeter.refuses(path) {
            return Err(PermitDenied::Read { path: path.clone() });
        }
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
        if self.read_perimeter.refuses(path) {
            return Err(PermitDenied::List { path: path.clone() });
        }
        Self::allows(&self.scope.list_paths, path, |path| PermitDenied::List {
            path,
        })
    }

    /// Checks a search against [`Scope::search_paths`].
    pub fn allows_search(&self, path: &CanonPath) -> Result<(), PermitDenied> {
        if self.read_perimeter.refuses(path) {
            return Err(PermitDenied::Search { path: path.clone() });
        }
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

    /// Checks the whole process specification against the one policy admitted.
    pub fn allows_spawn(&self, spec: &SpawnSpec) -> Result<(), PermitDenied> {
        if self.scope.spawns.contains(spec) {
            Ok(())
        } else {
            Err(PermitDenied::Spawn {
                spec: Box::new(spec.clone()),
            })
        }
    }

    /// Checks one exact session evidence reference and byte offset.
    pub fn allows_evidence(&self, request: &EvidenceRead) -> Result<(), PermitDenied> {
        if self.scope.evidence.as_ref() == Some(request) {
            Ok(())
        } else {
            Err(PermitDenied::Evidence {
                request: request.clone(),
            })
        }
    }

    pub fn allows_session_process(
        &self,
        id: DetachedExecId,
        action: SessionProcessAction,
    ) -> Result<(), PermitDenied> {
        if self.scope.session_process == Some((id, action)) {
            Ok(())
        } else {
            Err(PermitDenied::SessionProcess { id, action })
        }
    }

    pub(crate) fn allows_foreign(
        &self,
        invocations: &[(ForeignToolId, InvocationDigest)],
        transport: &ForeignTransportTarget,
    ) -> Result<(), TransportError> {
        if self.scope.foreign_invocations == invocations
            && self.scope.foreign_transport.as_ref() == Some(transport)
        {
            Ok(())
        } else {
            Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))
        }
    }

    pub(crate) fn allows_network(&self, connect: &PreparedConnect) -> Result<(), TransportError> {
        if self.scope.network.as_ref() == Some(connect.target())
            && self.scope.http_request_digest.as_ref() == Some(connect.request_digest())
        {
            Ok(())
        } else {
            Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))
        }
    }

    pub(crate) fn allows_secret_egress(
        &self,
        authority: &super::SecretEgressAuthority,
    ) -> Result<(), TransportError> {
        if self.scope.secret_egress.as_ref() == Some(authority) {
            Ok(())
        } else {
            Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))
        }
    }

    pub(crate) fn allows_protocol_network(
        &self,
        target: &NetworkTarget,
    ) -> Result<(), TransportError> {
        if self.scope.network.as_ref() == Some(target) && self.scope.http_request_digest.is_none() {
            Ok(())
        } else {
            Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))
        }
    }
}

/// A call and the grant that answers it, as one value.
///
/// Pair a permit with its prepared call structurally. A caller must not be
/// able to combine the grant for call A with call B.
///
/// No `Clone` and no public constructor: an `Authorized` is produced by
/// [`PolicyEngine::classify`] and by nothing else.
///
/// # Leg 1, continued
///
/// `Authorized` exposes borrows and `into_parts`, but its fields and
/// constructor remain private. The shared trybuild fixture attempts the
/// forbidden field construction from an external crate. Pairing a call with
/// a permit is therefore structural at construction time; after `into_parts`,
/// `GuardedEffects` still limits every operation to the permit scope.
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
    /// for is [`GuardedEffects`](super::GuardedEffects), which binds one permit
    /// to the raw effect ports and rechecks every operation at use time.
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
    pub fn into_parts(mut self) -> (PreparedCall, Permit) {
        self.permit.call_fingerprint = Some(self.call.authority_fingerprint());
        (self.call, self.permit)
    }

    pub(crate) fn from_verified_resume(call: PreparedCall, by: Authority) -> Self {
        let permit = Permit {
            scope: scope_of(&call),
            read_perimeter: super::ReadPerimeter::default(),
            call_fingerprint: None,
            _sealed: Sealed(()),
        };
        Self { call, permit, by }
    }

    pub(crate) fn with_verified_resume_parameters(
        mut self,
        parameters: &serde_json::Value,
    ) -> Result<Self, crate::mcp::McpProjectionError> {
        self.call.apply_verified_resume_parameters(parameters)?;
        Ok(self)
    }
}

/// Why policy refused to authorise a call.
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
    /// Include rule origins with the ID. Equal predicates share an ID across
    /// layers, but the operator needs provenance to know which source can lift
    /// a denial before the run exits.
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
    ProtectedRead {
        tool: String,
        op: OpName,
        path: CanonPath,
    },
    OutsideWorkspace {
        tool: String,
        op: OpName,
        path: CanonPath,
    },
    /// The path is inside a read-only profile root, and the operation
    /// changes something. No mode lifts it, like the workspace boundary.
    ReadOnlyRoot {
        tool: String,
        op: OpName,
        path: CanonPath,
    },
    /// An asked party refused. Distinguish this from a matching deny rule so operators are not
    /// sent looking for configuration nobody wrote. `by` records the actual party.
    Refused {
        tool: String,
        op: OpName,
        path: CanonPath,
        by: Authority,
    },
    /// A non-filesystem operation denied by a rule.
    DeniedSubject {
        tool: String,
        op: OpName,
        subject: Subject,
        by: RuleId,
        from: Source,
        also: OriginSet,
    },
    /// A non-filesystem operation forbidden by the mode floor.
    ModeForbidsSubject {
        tool: String,
        mode: Mode,
        op: OpName,
        subject: Subject,
    },
    /// A party refused a non-filesystem operation.
    RefusedSubject {
        tool: String,
        op: OpName,
        subject: Subject,
        by: Authority,
    },
    /// Session evidence authority is valid only as the call's sole operation.
    EvidenceMustStandAlone { tool: String },
    /// The human declined the call and said what to do instead. The only
    /// refusal the model is told about: the words are the answer, so the run
    /// continues with them as the tool result.
    Redirected { tool: String, instruction: String },
}

impl PolicyDenied {
    /// A read, list or search the workspace boundary refused.
    ///
    /// The caller returns it to the model as a failed tool result and the turn goes on.
    pub fn is_unreachable_read(&self) -> bool {
        matches!(
            self,
            PolicyDenied::OutsideWorkspace {
                op: OpName::Read | OpName::List | OpName::Search,
                ..
            }
        )
    }

    /// Who refused, for `EventKind::PermissionDecided`.
    ///
    /// One authority in every arm, and that is the shape of a refusal rather
    /// than a simplification: `judge` stops at the first step that says no, so
    /// exactly one thing did. The list in [`Authority::Rules`] is a list for
    /// the *allow* case, where nothing stops early.
    pub fn authority(&self) -> Authority {
        match self {
            PolicyDenied::Denied { by, from, also, .. }
            | PolicyDenied::DeniedSubject { by, from, also, .. } => Authority::Rules {
                rules: vec![RuleAuthority {
                    id: by.clone(),
                    from: *from,
                    also: *also,
                }],
            },
            PolicyDenied::ModeForbids { mode, .. }
            | PolicyDenied::ModeForbidsSubject { mode, .. } => Authority::Mode { mode: *mode },
            PolicyDenied::OutsideWorkspace { .. }
            | PolicyDenied::ProtectedRead { .. }
            | PolicyDenied::ReadOnlyRoot { .. } => Authority::Boundary,
            PolicyDenied::Refused { by, .. } | PolicyDenied::RefusedSubject { by, .. } => {
                by.clone()
            }
            PolicyDenied::EvidenceMustStandAlone { .. } => Authority::SessionEvidence,
            PolicyDenied::Redirected { .. } => Authority::Party,
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
                    Source::Default => write!(
                        f,
                        "credential defaults; an explicit user read rule can lift this preset"
                    )?,
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
            PolicyDenied::ProtectedRead { tool, op, path } => {
                write!(f, "{tool} may not {op} {path}: protected credential path")
            }
            PolicyDenied::OutsideWorkspace { tool, op, path } => {
                write!(f, "{tool} may not {op} {path}: it is outside the workspace")?;
                // A read the shell could still do under Adaptive: name the
                // grant, because the model otherwise falls back to `cat`.
                // Not for credentials: the defaults keep denying those.
                if self.is_unreachable_read() && !Ruleset::is_credential_path(path) {
                    let quoted_path = path.to_string().replace('\'', "'\\''");
                    write!(
                        f,
                        "; ask the user to run `ego roots add --access read '{quoted_path}'`, \
                         then exit and resume the session"
                    )?;
                } else if *op == OpName::Write && !Ruleset::is_credential_path(path) {
                    let quoted_path = path.to_string().replace('\'', "'\\''");
                    write!(
                        f,
                        "; ask the user to run `ego roots add --access read-write '{quoted_path}'`, \
                         then exit and resume the session"
                    )?;
                }
                Ok(())
            }
            PolicyDenied::ReadOnlyRoot { tool, op, path } => {
                write!(
                    f,
                    "{tool} may not {op} {path}: it is inside a read-only root"
                )
            }
            PolicyDenied::Refused { tool, op, path, .. } => {
                write!(f, "{tool} may not {op} {path}: permission was refused")
            }
            PolicyDenied::DeniedSubject {
                tool,
                op,
                subject,
                by,
                from,
                ..
            } => write!(
                f,
                "{tool} may not {op} {subject}: denied by {by} in the {from} config"
            ),
            PolicyDenied::ModeForbidsSubject {
                tool,
                mode,
                op,
                subject,
            } => write!(
                f,
                "{tool} may not {op} {subject}: {mode} mode forbids every operation that changes anything"
            ),
            PolicyDenied::RefusedSubject {
                tool, op, subject, ..
            } => write!(f, "{tool} may not {op} {subject}: permission was refused"),
            PolicyDenied::EvidenceMustStandAlone { tool } => write!(
                f,
                "{tool} may not combine session evidence with another operation"
            ),
            PolicyDenied::Redirected { tool, instruction } => write!(
                f,
                "{tool} was not run: the user declined it and said: {instruction}"
            ),
        }
    }
}

impl std::error::Error for PolicyDenied {}

/// Why a call could not be settled without asking somebody.
///
/// Call-scoped permit construction remains private.
///
/// Terminal settlement errors remain distinct from pending approver states.
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
    /// The mode does not auto-approve the operation. Produced by `ModeApprover`.
    ModeAsks {
        mode: Mode,
        op: OpName,
        path: CanonPath,
    },
    /// No allow rule covered a non-filesystem operation.
    NoRuleCoversSubject { op: OpName, subject: Subject },
    /// The mode declined a non-filesystem operation.
    ModeAsksSubject {
        mode: Mode,
        op: OpName,
        subject: Subject,
    },
}

impl Escalation {
    /// The operation the escalation is about. Every arm names one, because a
    /// question that cannot say what it is about is not a question anybody
    /// can answer.
    pub fn op(&self) -> OpName {
        match self {
            Escalation::NoRuleCovers { op, .. }
            | Escalation::ModeAsks { op, .. }
            | Escalation::NoRuleCoversSubject { op, .. }
            | Escalation::ModeAsksSubject { op, .. } => *op,
        }
    }

    pub fn path(&self) -> &CanonPath {
        match self {
            Escalation::NoRuleCovers { path, .. } | Escalation::ModeAsks { path, .. } => path,
            Escalation::NoRuleCoversSubject { .. } | Escalation::ModeAsksSubject { .. } => {
                panic!("a non-filesystem escalation has no path")
            }
        }
    }

    pub fn subject(&self) -> Subject {
        match self {
            Escalation::NoRuleCovers { path, .. } | Escalation::ModeAsks { path, .. } => {
                Subject::Path(path.clone())
            }
            Escalation::NoRuleCoversSubject { subject, .. }
            | Escalation::ModeAsksSubject { subject, .. } => subject.clone(),
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
            Escalation::NoRuleCoversSubject { op, subject } => {
                write!(f, "no rule covers {op} {subject}")
            }
            Escalation::ModeAsksSubject { mode, op, subject } => {
                write!(f, "{mode} mode does not approve {op} {subject} on its own")
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
    presentation: Option<Box<PermissionPresentation>>,
    child_name: Option<String>,
    explicit_answer: bool,
}

/// Display-only facts retained for a human permission decision.
///
/// This value deliberately contains no permit, scope, config store, mutation
/// handle, or policy-selection authority. The parameters and schema are the
/// exact admitted input, while risk and rule candidates are derived from the
/// already-canonical prepared call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionPresentation {
    parameters: Value,
    schema: Value,
    risk: super::analysis::RiskAssessment,
    remember: Result<RememberPlan, String>,
    suggested: Result<RememberPlan, String>,
    /// Why the judge passed the call to the human, when it did. Model text:
    /// show it as untrusted words, never as a decision.
    judge_reason: Option<String>,
}

impl PermissionPresentation {
    pub fn judge_reason(&self) -> Option<&str> {
        self.judge_reason.as_deref()
    }

    pub fn parameters(&self) -> &Value {
        &self.parameters
    }

    pub fn schema(&self) -> &Value {
        &self.schema
    }

    pub fn risk(&self) -> &super::analysis::RiskAssessment {
        &self.risk
    }

    /// The exact rule for each operation of the call.
    pub fn remember_plan(&self) -> Result<&RememberPlan, &str> {
        self.remember.as_ref().map_err(String::as_str)
    }

    /// The rules offered to a human who asks not to be asked again; see
    /// [`RuleManagement::suggest_remember`].
    pub fn suggested_remember_plan(&self) -> Result<&RememberPlan, &str> {
        self.suggested.as_ref().map_err(String::as_str)
    }
}

/// The exact admitted input before policy combines it with canonical effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionPresentationInput {
    parameters: Value,
    schema: Value,
}

impl PermissionPresentationInput {
    pub(crate) fn new(parameters: Value, schema: Value) -> Self {
        Self { parameters, schema }
    }
}

impl PermissionRequest {
    /// Build the question an adapter has to render.
    ///
    /// Public because a host seat outside this crate — the ACP adapter — has
    /// to be able to construct one to prove it maps every field onto the
    /// wire. This grants nothing: `tool`, `ops` and `why` are already fully
    /// readable through the accessors below, and the [`Scope`] the doc
    /// comment above withholds is still not a field. Producing a request has
    /// never been authority; only `PolicyEngine::settle` mints a permit.
    pub fn new(tool: impl Into<String>, ops: Vec<Op>, why: Escalation) -> Self {
        Self {
            tool: tool.into(),
            ops,
            why,
            presentation: None,
            child_name: None,
            explicit_answer: false,
        }
    }

    /// The child requesting this permission, when the call is delegated.
    pub fn child_name(&self) -> Option<&str> {
        self.child_name.as_deref()
    }

    /// Reconstructed session authority needs a fresh answer, even when a
    /// previous tool request received a session-wide approval.
    pub fn execution_policy_readmission(message: String, workspace: CanonPath) -> Self {
        let mut request = Self::new(
            format!("Re-admit execution policy: {message}"),
            Vec::new(),
            Escalation::NoRuleCovers {
                op: super::OpName::Opaque,
                path: workspace,
            },
        );
        request.explicit_answer = true;
        request
    }

    /// A separate path capability, never a remembered command permission.
    pub fn sandbox_grant(grant: &super::SandboxGrant, command: &str) -> Self {
        let mut request = Self::new(
            format!(
                "sandbox {grant} for this session. The command will run again and may repeat prior effects: {command}"
            ),
            Vec::new(),
            Escalation::NoRuleCovers {
                op: super::OpName::Opaque,
                path: grant.path.clone(),
            },
        );
        request.explicit_answer = true;
        request
    }

    pub const fn requires_explicit_answer(&self) -> bool {
        self.explicit_answer
    }

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

    pub fn presentation(&self) -> Option<&PermissionPresentation> {
        self.presentation.as_deref()
    }
}

impl fmt::Display for PermissionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let why = self.why.to_string();
        if let Some(name) = &self.child_name {
            write!(f, "Child {}: ", TerminalSafe(name))?;
        }
        write!(
            f,
            "{} needs permission: {}",
            TerminalSafe(&self.tool),
            TerminalSafe(&why)
        )?;
        for op in &self.ops {
            match op {
                Op::Read { path } => {
                    write!(f, "\n- read {}", TerminalSafe(&path.to_string()))?;
                }
                Op::List { root, pattern } => {
                    write!(
                        f,
                        "\n- list {} matching {}",
                        TerminalSafe(&root.to_string()),
                        TerminalSafe(pattern)
                    )?;
                }
                Op::Search { root, query } => {
                    write!(
                        f,
                        "\n- search {} for {}",
                        TerminalSafe(&root.to_string()),
                        TerminalSafe(query)
                    )?;
                }
                Op::Write { path, change } => {
                    write!(f, "\n- write {}", TerminalSafe(&path.to_string()))?;
                    match change {
                        Change::Notebook(edit) => write!(
                            f,
                            "\n  notebook cell edit: {}\n  other cells, metadata and outputs are retained",
                            TerminalSafe(
                                &serde_json::to_string(edit).expect("closed cell edit codec")
                            )
                        )?,
                        Change::Whole(content) => write!(
                            f,
                            "\n  whole content ({} bytes): {}",
                            content.len(),
                            EscapedBytes(content)
                        )?,
                        Change::Replace {
                            replace_all,
                            old_string,
                            new_string,
                        } => write!(
                            f,
                            "\n  replace {} old text: {}\n  with new text: {}\n  final content depends on the file at execution time",
                            if *replace_all {
                                "all original non-overlapping occurrences of"
                            } else {
                                "exactly one occurrence of"
                            },
                            TerminalSafe(old_string),
                            TerminalSafe(new_string)
                        )?,
                    }
                }
                Op::Spawn { spec } => {
                    write!(f, "\n- spawn {:?}", spec.argv())?;
                }
                Op::ReadEvidence { request } => {
                    write!(
                        f,
                        "\n- read session evidence {} at byte {}",
                        request.reference, request.offset
                    )?;
                }
                Op::SessionProcess { id, action } => {
                    write!(f, "\n- session process {id} {action:?}")?;
                }
                Op::Opaque {
                    tool,
                    params_digest,
                    transport,
                } => write!(
                    f,
                    "\n- invoke {} ({}) over {}",
                    TerminalSafe(&tool.to_string()),
                    params_digest,
                    TerminalSafe(&transport.to_string())
                )?,
                Op::Connect { target, .. } | Op::ProtocolConnect { target } => {
                    write!(f, "\n- connect {}", TerminalSafe(&target.to_string()))?;
                }
                Op::SecretEgress { authority } => write!(
                    f,
                    "\n- release secrets {:?} to {} headers {:?}",
                    authority.sources(),
                    TerminalSafe(&authority.target().to_string()),
                    authority.header_sinks()
                )?,
            }
        }
        Ok(())
    }
}

/// A lossless byte rendering for proposed whole-file content.
struct EscapedBytes<'a>(&'a [u8]);

impl fmt::Display for EscapedBytes<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            for escaped in std::ascii::escape_default(*byte) {
                f.write_char(char::from(escaped))?;
            }
        }
        Ok(())
    }
}

/// An unsettled call with sealed fields and no `Clone`. Only the engine constructs it and
/// converts it into a permit. Scope is re-derived from the sealed call, never nominated by the
/// loop or approver.
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

    fn with_escalation(mut self, why: Escalation) -> Self {
        self.ask.why = why;
        self
    }

    fn with_judge_reason(mut self, reason: &str) -> Self {
        if let Some(presentation) = self.ask.presentation.as_mut() {
            presentation.judge_reason = Some(reason.to_owned());
        }
        self
    }
}

/// What [`PolicyEngine::classify`] concluded.
///
/// Two arms because "the answer is settled by rules" and "the next policy
/// party must be consulted" are different facts.
///
/// The size difference between the variants is deliberate. A classification
/// is produced once per call and consumed immediately; it is never retained
/// or collected. Boxing the settled result would add a heap allocation to
/// every decided authorization only to shrink this short-lived stack value.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Classification {
    /// Decided, one way or the other, without asking anybody.
    Settled(Result<Authorized, PolicyDenied>),
    /// Continue through the ordered policy parties.
    Ask(Pending),
}

/// What a [`PolicyEngine`] concluded after consulting one policy party.
///
/// `Ask` means the current party escalated. The turn records that question
/// before it asks the human seat, if one exists. `Reclassify` carries raw
/// params and never a scope or prepared call.
/// Like `Classification`, this short-lived decision keeps the prepared call
/// inline; adding human display context does not warrant boxing authorizations.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Resolution {
    /// Decided. The party allowed it, or refused it.
    Settled(Result<Authorized, PolicyDenied>),
    /// Still open, with the reason this party gave for not answering.
    Ask(Pending),
    /// A human answered Allow or Deny. The request travels with the result so
    /// later audit facts identify the exact canonical question answered.
    HumanSettled {
        request: PermissionRequest,
        result: Result<Authorized, PolicyDenied>,
    },
    /// A human replaced the tool's raw parameters. No prepared call and no
    /// scope crosses this seam: the turn must prepare and classify again.
    Reclassify {
        request: PermissionRequest,
        params: serde_json::Value,
    },
}

/// Classify every hard refusal before consulting approvers.
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
/// `mint` and the mode approver remain private, and callers cannot compose a
/// `Scope` through its fields. The trybuild privacy contract covers these
/// external field, type, and method boundaries; the declarations here directly
/// enforce the private methods and module. There is deliberately no `widen`
/// method: broader authority requires a new classification and a newly minted
/// `Permit`.
///
/// Callers may still select `Mode::Yolo` through configuration. Given the
/// resolved mode, no downstream code can replace its approver. Configuration
/// provenance is covered by `tests/config.rs`.
pub struct PolicyEngine {
    /// The boundary ego draws for itself. Not a rule: nobody wrote it, and no
    /// rule lifts it.
    workspace: CanonPath,
    /// Extra roots a profile opened, beyond the workspace. Empty for a run
    /// with no profile roots, which keeps the single-boundary behaviour.
    roots: Vec<ProfileRoot>,
    read_perimeter: super::ReadPerimeter,
    mode: Mode,
    /// Both lists intact, matched per call. `resolve` no longer subtracts one
    /// from the other — that comparison was string equality between two lists
    /// and `deny = ["exec(git *)"]` left `allow = ["exec(git push)"]` standing.
    allow: Vec<SourcedRule>,
    deny: Vec<SourcedRule>,
    /// Mode approval is built by `new` from resolved configuration, not injected by the caller.
    /// See `ModeApprover` for the visibility boundary.
    approver: Box<dyn Approver>,
    /// The final party, absent in headless runs. It is separate from
    /// `approver` so no caller can replace the mode derived from config.
    human: Option<Arc<dyn HumanApprover>>,
}

impl fmt::Debug for PolicyEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PolicyEngine")
            .field("mode", &self.mode)
            .field("human_present", &self.human.is_some())
            .field("policy", &"<redacted>")
            .finish()
    }
}

impl PolicyEngine {
    /// A policy that decides `mode`'s way, under `permissions`, inside
    /// `workspace`.
    pub fn new(workspace: CanonPath, mode: Mode, permissions: Ruleset) -> Self {
        let permissions = permissions.with_credential_defaults();
        Self {
            approver: Box::new(ModeApprover::new(mode, workspace.clone())),
            workspace,
            roots: Vec::new(),
            read_perimeter: super::ReadPerimeter::default(),
            mode,
            allow: permissions.allow,
            deny: permissions.deny,
            human: None,
        }
    }

    /// The same engine with the profile's admitted roots.
    ///
    /// A path is judged by the most specific of the workspace and the roots
    /// that contains it, so a read-write root may sit inside a read root and
    /// a read root inside a read-write one. The workspace is always
    /// read-write.
    pub fn with_profile_roots(self, roots: Vec<ProfileRoot>) -> Self {
        Self { roots, ..self }
    }

    /// Pin the read authority derived from the admitted run confinement.
    pub fn with_read_perimeter(self, read_perimeter: super::ReadPerimeter) -> Self {
        Self {
            read_perimeter,
            ..self
        }
    }

    pub(crate) fn judge_roots(&self) -> Vec<String> {
        std::iter::once(self.workspace.to_string())
            .chain(
                self.roots
                    .iter()
                    .map(|root| format!("{} ({:?})", root.path(), root.access())),
            )
            .collect()
    }

    pub(crate) fn risk(&self, call: &PreparedCall) -> super::analysis::RiskAssessment {
        use super::analysis::RiskReason;
        let mut risk = super::analysis::assess(call);
        // The prepared paths are canonical; a tool name says nothing about
        // whether its writes land inside this workspace.
        risk.reasons
            .retain(|reason| *reason != RiskReason::ScopedWrite);
        for op in call.ops() {
            if let Op::Write { path, .. } = op {
                risk.reasons.push(if path.is_or_under(&self.workspace) {
                    RiskReason::WorkspaceWrite
                } else {
                    RiskReason::OutsideWorkspaceWrite
                });
            }
        }
        risk.reasons.sort();
        risk.reasons.dedup();
        risk
    }

    /// Nested read-only restrictions veto writes throughout their subtree.
    /// Otherwise the most specific root wins. Parent traversal is not canonical.
    fn reach(&self, path: &CanonPath) -> Option<RootAccess> {
        if path
            .as_path()
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        {
            return None;
        }
        if self.roots.iter().any(|root| {
            root.restricts_writes(&self.workspace, &self.roots) && path.is_or_under(root.path())
        }) {
            return Some(RootAccess::Read);
        }
        let depth = |base: &CanonPath| base.as_path().components().count();
        self.roots
            .iter()
            .map(|root| (root.path(), root.access()))
            .chain(std::iter::once((&self.workspace, RootAccess::ReadWrite)))
            .filter(|(base, _)| path.is_or_under(base))
            .max_by_key(|(base, _)| depth(base))
            .map(|(_, access)| access)
    }

    /// Classify an effect call while retaining only the display facts needed
    /// if it reaches the human party.
    pub(crate) fn classify_presented(
        &self,
        call: PreparedCall,
        input: PermissionPresentationInput,
    ) -> Classification {
        match self.classify(call) {
            Classification::Ask(mut pending) => {
                let represent = |plan: Result<RememberPlan, _>| {
                    plan.map_err(
                        |error: crate::config::rule_management::RuleManagementError| {
                            format!("this call cannot be represented by a persistent rule: {error}")
                        },
                    )
                };
                let remember = represent(RuleManagement::remember(
                    &pending.call,
                    ManagedRuleEffect::Allow,
                    self.workspace.as_path(),
                ));
                let suggested = represent(RuleManagement::suggest_remember(
                    &pending.call,
                    ManagedRuleEffect::Allow,
                    self.workspace.as_path(),
                ));
                pending.ask.presentation = Some(Box::new(PermissionPresentation {
                    parameters: input.parameters,
                    schema: input.schema,
                    risk: self.risk(&pending.call),
                    remember,
                    suggested,
                    judge_reason: None,
                }));
                Classification::Ask(pending)
            }
            settled => settled,
        }
    }

    /// The immutable mode this engine was constructed to enforce.
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    pub fn resume_floor(&self) -> crate::suspension::ResumeFloor {
        crate::suspension::ResumeFloor::from_policy(
            self.workspace.to_string(),
            self.roots.clone(),
            self.mode,
            self.deny
                .iter()
                .map(|entry| entry.rule().id().to_string())
                .collect(),
        )
    }

    /// A policy with a human final party after the mode ceiling.
    ///
    /// Named construction rather than a setter or generic approver list: the
    /// order is policy, not host preference, and the mode remains the one
    /// resolved from config.
    pub fn with_human(
        workspace: CanonPath,
        mode: Mode,
        permissions: Ruleset,
        human: Arc<dyn HumanApprover>,
    ) -> Self {
        Self {
            human: Some(human),
            ..Self::new(workspace, mode, permissions)
        }
    }

    /// Test-only alternative approver exercises verdicts the production mode does not produce.
    /// This constructor is absent from release builds and inaccessible to other crates.
    #[cfg(test)]
    fn with_approver(self, approver: Box<dyn Approver>) -> Self {
        Self { approver, ..self }
    }

    /// Decide what a prepared call may do and return its corresponding grant.
    /// Derive scope from the call and return it inside [`Authorized`], preserving
    /// the structural permit/call pairing. Classification is pure and synchronous.
    /// Check boundary, deny and mode floor for every operation before any operation
    /// may ask an approver. Otherwise an early ordinary question could hide a later
    /// out-of-boundary operation and approval would authorize the whole call.
    /// After hard refusals, explicit allows and [`settle`](Self::settle) handle
    /// unresolved operations. System denies cannot be cleared by higher layers.
    /// Supporting foreign transport may reuse an already admitted delegation,
    /// never bypass an independent operation's decision.
    pub fn classify(&self, call: PreparedCall) -> Classification {
        if matches!(call.ops(), [Op::ReadEvidence { .. }]) {
            let permit = self.mint(scope_of(&call));
            return Classification::Settled(Ok(Authorized {
                call,
                permit,
                by: Authority::SessionEvidence,
            }));
        }
        if matches!(call.ops(), [Op::SessionProcess { .. }]) {
            let permit = self.mint(scope_of(&call));
            return Classification::Settled(Ok(Authorized {
                call,
                permit,
                by: Authority::SessionProcess,
            }));
        }
        if call
            .ops()
            .iter()
            .any(|op| matches!(op, Op::ReadEvidence { .. }))
        {
            return Classification::Settled(Err(PolicyDenied::EvidenceMustStandAlone {
                tool: call.into_parts().0,
            }));
        }
        for op in call.ops() {
            if let Err(denied) = self.judge(&call, op) {
                return Classification::Settled(Err(*denied));
            }
        }
        // Every rule that covered an independent op, not the first one to
        // match. A foreign transport structurally supporting wholly allowed
        // opaque identities contributes no second rule authority. A denial is
        // the other shape and gets one authority, because `judge` stops at the
        // first deny.
        let delegated_support = delegated_supporting_op(&call).filter(|_| {
            call.ops()
                .iter()
                .filter(|op| matches!(op, Op::Opaque { .. }))
                .all(|op| self.matching_allow(op).is_some())
        });
        let mut rules: Vec<RuleAuthority> = Vec::new();
        let mut yolo_authorized_secret_egress = false;
        for (index, op) in call.ops().iter().enumerate() {
            if delegated_support == Some(index) {
                continue;
            }
            // A conditional replacement reads the same file it writes. Its
            // matching write allow covers that supporting read, but the first
            // pass still checks read denies and boundaries independently.
            let supporting_write = match (call.ops(), op) {
                (
                    [
                        Op::Read { path: read },
                        write @ Op::Write {
                            path,
                            change: Change::Replace { .. } | Change::Notebook(_),
                        },
                    ],
                    Op::Read { .. },
                ) if read == path => Some(write),
                _ => None,
            };
            let Some(entries) = self
                .matching_allow(op)
                .or_else(|| supporting_write.and_then(|write| self.matching_allow(write)))
            else {
                if self.mode == Mode::Yolo && matches!(op, Op::SecretEgress { .. }) {
                    yolo_authorized_secret_egress = true;
                    continue;
                }
                let why = match op.subject_path(&self.workspace) {
                    Some(path) => Escalation::NoRuleCovers {
                        op: op.name(),
                        path: path.clone(),
                    },
                    None => Escalation::NoRuleCoversSubject {
                        op: op.name(),
                        subject: op.subject(&self.workspace),
                    },
                };
                return Classification::Ask(self.pending(call, why));
            };
            for entry in entries {
                let authority = RuleAuthority::of(entry);
                if !rules.contains(&authority) {
                    rules.push(authority);
                }
            }
        }
        let permit = self.mint(scope_of(&call));
        Classification::Settled(Ok(Authorized {
            call,
            permit,
            by: if yolo_authorized_secret_egress {
                Authority::Mode { mode: Mode::Yolo }
            } else {
                Authority::Rules { rules }
            },
        }))
    }

    /// Consult the next policy party and interpret its verdict. Only the engine derives permit
    /// scope. Escalation returns a question that the turn must persist before asking the human,
    /// so a crash cannot erase the request.
    pub async fn settle(&self, pending: Pending) -> Resolution {
        match self.approver.approve(&pending).await {
            Verdict::Allow { by } => {
                let call = pending.call;
                let permit = self.mint(scope_of(&call));
                Resolution::Settled(Ok(Authorized { call, permit, by }))
            }
            Verdict::Deny { by } => Resolution::Settled(Err(refused(
                pending.call.into_parts().0,
                &pending.ask.why,
                by,
            ))),
            Verdict::Escalate { why } => {
                if self.mode == Mode::Auto
                    && (self.risk(pending.call()).class <= super::analysis::RiskClass::Bounded
                        || pending
                            .call()
                            .ops()
                            .iter()
                            .all(|operation| match operation {
                                Op::Spawn { spec } => super::shell::trusted_read_only(spec),
                                _ => false,
                            }))
                {
                    let call = pending.call;
                    let permit = self.mint(scope_of(&call));
                    Resolution::Settled(Ok(Authorized {
                        call,
                        permit,
                        by: Authority::Mode { mode: self.mode },
                    }))
                } else {
                    Resolution::Ask(pending.with_escalation(why))
                }
            }
        }
    }

    /// Apply one judge opinion without exposing a new policy resolution shape.
    /// The sealed pending call remains the sole source of the minted scope.
    pub(crate) fn settle_judge(
        &self,
        pending: Pending,
        outcome: &super::judge::JudgeOutcome,
        by: Authority,
    ) -> Resolution {
        let pending = match outcome {
            super::judge::JudgeOutcome::Escalate { reason } => pending.with_judge_reason(reason),
            _ => pending,
        };
        if self.mode != Mode::Auto
            || self.risk(pending.call()).class == super::analysis::RiskClass::High
        {
            return Resolution::Ask(pending);
        }
        match outcome {
            super::judge::JudgeOutcome::Allow { .. } => {
                let call = pending.call;
                let permit = self.mint(scope_of(&call));
                Resolution::Settled(Ok(Authorized { call, permit, by }))
            }
            super::judge::JudgeOutcome::Deny { .. } => Resolution::Settled(Err(refused(
                pending.call.into_parts().0,
                &pending.ask.why,
                by,
            ))),
            super::judge::JudgeOutcome::Escalate { .. } => Resolution::Ask(pending),
        }
    }

    /// Ask the terminal party after the turn has recorded the request.
    ///
    /// Crate-private because only the turn loop owns the durable ordering
    /// boundary. A headless engine preserves the pending question unchanged.
    pub(crate) async fn decide(&self, item: ItemId, pending: Pending) -> Resolution {
        self.decide_named(item, pending, None).await
    }

    pub(crate) async fn decide_named(
        &self,
        item: ItemId,
        pending: Pending,
        child_name: Option<&str>,
    ) -> Resolution {
        let Some(human) = &self.human else {
            return Resolution::Ask(pending);
        };
        let mut request = pending.request().clone();
        request.child_name = child_name.map(str::to_owned);
        match human.decide(item, &request).await {
            PermissionDecision::Allow => {
                let call = pending.call;
                let permit = self.mint(scope_of(&call));
                Resolution::HumanSettled {
                    request,
                    result: Ok(Authorized {
                        call,
                        permit,
                        by: Authority::Party,
                    }),
                }
            }
            PermissionDecision::Deny => Resolution::HumanSettled {
                request,
                result: Err(refused(
                    pending.call.into_parts().0,
                    &pending.ask.why,
                    Authority::Party,
                )),
            },
            PermissionDecision::DenyWithInstruction { instruction } => Resolution::HumanSettled {
                request,
                result: Err(PolicyDenied::Redirected {
                    tool: pending.call.into_parts().0,
                    instruction,
                }),
            },
            PermissionDecision::Amend { params } => Resolution::Reclassify { request, params },
        }
    }

    /// The only constructor of `Pending`. Keeping it private prevents callers from choosing
    /// their own approval party.
    fn pending(&self, call: PreparedCall, why: Escalation) -> Pending {
        Pending {
            ask: PermissionRequest {
                tool: call.tool().to_owned(),
                ops: call.ops().to_vec(),
                why,
                presentation: None,
                child_name: None,
                explicit_answer: false,
            },
            call,
        }
    }

    /// Steps 0 to 2: everything that can refuse an operation outright.
    fn judge(&self, call: &PreparedCall, op: &Op) -> Result<(), Box<PolicyDenied>> {
        let path = op.subject_path(&self.workspace);

        // 0. The workspace boundary. Evaluated before the rules because it is not one of
        // them: no operator wrote it, `--allow` cannot widen it, and `yolo` does not lift
        // it.
        if let Some(path) = path
            && !matches!(op, Op::Spawn { .. })
        {
            let is_read = matches!(op, Op::Read { .. } | Op::List { .. } | Op::Search { .. });
            if is_read && self.read_perimeter.refuses(path) {
                return Err(Box::new(PolicyDenied::ProtectedRead {
                    tool: call.tool().to_owned(),
                    op: op.name(),
                    path: path.clone(),
                }));
            }
            match self
                .reach(path)
                .or_else(|| (is_read && self.read_perimeter.broad()).then_some(RootAccess::Read))
            {
                None => {
                    return Err(Box::new(PolicyDenied::OutsideWorkspace {
                        tool: call.tool().to_owned(),
                        op: op.name(),
                        path: path.clone(),
                    }));
                }
                Some(RootAccess::Read) if op.mutates() => {
                    return Err(Box::new(PolicyDenied::ReadOnlyRoot {
                        tool: call.tool().to_owned(),
                        op: op.name(),
                        path: path.clone(),
                    }));
                }
                Some(_) => {}
            }
        }

        // 1. Deny, before any allow at any scope.
        if let Some(entry) = self.matching(&self.deny, op) {
            let (from, also) = entry.attribution();
            return Err(Box::new(match path {
                Some(path) => PolicyDenied::Denied {
                    tool: call.tool().to_owned(),
                    op: op.name(),
                    path: path.clone(),
                    by: entry.rule().id(),
                    from,
                    also,
                },
                None => PolicyDenied::DeniedSubject {
                    tool: call.tool().to_owned(),
                    op: op.name(),
                    subject: op.subject(&self.workspace),
                    by: entry.rule().id(),
                    from,
                    also,
                },
            }));
        }

        // 2. The mode floor. Above the allow list, so an allow rule cannot
        //    relax it: `plan` means the run does not change anything, and a
        //    mode a rule could switch off would not mean that.
        if self.mode == Mode::Plan && op.mutates() {
            return Err(Box::new(match path {
                Some(path) => PolicyDenied::ModeForbids {
                    tool: call.tool().to_owned(),
                    mode: self.mode,
                    op: op.name(),
                    path: path.clone(),
                },
                None => PolicyDenied::ModeForbidsSubject {
                    tool: call.tool().to_owned(),
                    mode: self.mode,
                    op: op.name(),
                    subject: op.subject(&self.workspace),
                },
            }));
        }

        Ok(())
    }

    fn matching_allow(&self, op: &Op) -> Option<Vec<&SourcedRule>> {
        SourcedRule::allowing(&self.allow, op, self.workspace.as_path())
    }

    /// The first rule in `rules` that covers `op`, if any.
    ///
    /// First match wins, in the order the lists were resolved. Matching is
    /// against the canonical operation and never against rendered text.
    fn matching<'r>(&self, rules: &'r [SourcedRule], op: &Op) -> Option<&'r SourcedRule> {
        rules
            .iter()
            .find(|entry| entry.denies(op, self.workspace.as_path(), &self.allow))
    }

    /// Mint a permit for an explicit scope.
    ///
    /// Private minting preserves call-scoped grants. Bypassing a prompt changes
    /// whether to ask, not what the call may access; callers must not construct
    /// an arbitrary wide `Scope` and mint authority for it.
    ///
    /// With it private, [`classify`](Self::classify) is the only path to a
    /// `Permit` in the entire program, and `classify` derives the scope from
    /// the call. A loop that chooses its own `Scope` is a loop that is its own
    /// policy, and now it cannot.
    fn mint(&self, scope: Scope) -> Permit {
        Permit {
            scope,
            read_perimeter: self.read_perimeter.clone(),
            call_fingerprint: None,
            _sealed: Sealed(()),
        }
    }
}

pub(super) struct ForeignClaim {
    pub(super) invocations: Vec<(ForeignToolId, InvocationDigest)>,
    pub(super) transport: ForeignTransportTarget,
    pub(super) connect: Option<PreparedConnect>,
    pub(super) protocol_target: Option<NetworkTarget>,
    pub(super) spawn: Option<SpawnSpec>,
}

impl ForeignClaim {
    pub(super) fn from_call(call: &PreparedCall) -> Option<Self> {
        let mut invocations = Vec::new();
        let mut transport = call
            .mcp_envelope()
            .map(|envelope| envelope.transport().clone());
        let mut connect = None;
        let mut protocol_target = None;
        let mut spawn = None;
        for op in call.ops() {
            match op {
                Op::Opaque {
                    tool,
                    params_digest,
                    transport: route,
                } => {
                    if transport.as_ref().is_some_and(|existing| existing != route) {
                        return None;
                    }
                    transport = Some(route.clone());
                    invocations.push((tool.clone(), params_digest.clone()));
                }
                Op::Connect {
                    target,
                    request_digest,
                } if connect.is_none() => {
                    connect = Some(PreparedConnect::from_parts(
                        target.clone(),
                        request_digest.clone(),
                    ));
                }
                Op::ProtocolConnect { target } if protocol_target.is_none() => {
                    protocol_target = Some(target.clone());
                }
                Op::Spawn { spec } if spawn.is_none() => spawn = Some(spec.clone()),
                _ => return None,
            }
        }
        if invocations.is_empty() && call.mcp_envelope().is_none() {
            return None;
        }
        Some(Self {
            invocations,
            transport: transport?,
            connect,
            protocol_target,
            spawn,
        })
    }
}

/// The one transport operation that only supports a structurally valid
/// foreign delegation.
///
/// This does not decide whether any identity is allowed. It narrows the
/// runtime's shared foreign-call projection to one compatible supporting
/// operation. Native and mixed-effect calls therefore have no operation to
/// collapse.
fn delegated_supporting_op(call: &PreparedCall) -> Option<usize> {
    if !call.has_captured_foreign_provenance() || call.hook_http_envelope().is_some() {
        return None;
    }
    let claim = ForeignClaim::from_call(call)?;
    if claim.invocations.is_empty() {
        return None;
    }

    let support_count = usize::from(claim.connect.is_some())
        + usize::from(claim.protocol_target.is_some())
        + usize::from(claim.spawn.is_some());
    if support_count != 1 {
        return None;
    }
    call.ops()
        .iter()
        .enumerate()
        .find_map(|(index, op)| match op {
            Op::Connect { target, .. }
                if call.mcp_envelope().is_none()
                    && claim
                        .transport
                        .http_route()
                        .is_some_and(|route| route.target() == target) =>
            {
                Some(index)
            }
            Op::ProtocolConnect { target }
                if call.mcp_envelope().is_some()
                    && claim
                        .transport
                        .http_route()
                        .is_some_and(|route| route.target() == target) =>
            {
                Some(index)
            }
            Op::Spawn { .. } if claim.transport.is_stdio() => Some(index),
            _ => None,
        })
}

fn refused(tool: String, why: &Escalation, by: Authority) -> PolicyDenied {
    match why {
        Escalation::NoRuleCovers { path, .. } | Escalation::ModeAsks { path, .. } => {
            PolicyDenied::Refused {
                tool,
                op: why.op(),
                path: path.clone(),
                by,
            }
        }
        Escalation::NoRuleCoversSubject { subject, .. }
        | Escalation::ModeAsksSubject { subject, .. } => PolicyDenied::RefusedSubject {
            tool,
            op: why.op(),
            subject: subject.clone(),
            by,
        },
    }
}

/// The authority a call's own operations contribute — **the** definition,
/// called once and never inlined at a second site.
///
/// Scope comes only from the prepared call. Matching rules cannot widen it.
/// Every operation has already been approved before scope construction.
///
/// Scoped to the paths this call named, not to the workspace: a tool that
/// asked to read one file is not thereby allowed to read the repository.
/// `GuardedIo` re-resolves at use time, so a symlink swapped in afterwards is
/// judged against this same narrow list.
fn scope_of(call: &PreparedCall) -> Scope {
    let mut scope = Scope::default();
    for op in call.ops() {
        // Each op widens only its own authority. A call that lists a
        // directory gets `list_paths` and no `read_paths`, so the tool learns
        // names and cannot then read what it found — the whole reason these
        // are separate lists.
        match op {
            Op::Read { path } => scope.read_paths.push(path.clone()),
            Op::List { root, .. } => scope.list_paths.push(root.clone()),
            Op::Search { root, .. } => scope.search_paths.push(root.clone()),
            Op::Write { path, .. } => scope.write_paths.push(path.clone()),
            Op::Spawn { spec } => scope.spawns.push(spec.clone()),
            Op::ReadEvidence { request } => scope.evidence = Some(request.clone()),
            Op::SessionProcess { id, action } => scope.session_process = Some((*id, *action)),
            Op::Opaque {
                tool,
                params_digest,
                transport,
            } => {
                scope
                    .foreign_invocations
                    .push((tool.clone(), params_digest.clone()));
                match &scope.foreign_transport {
                    Some(existing) if existing != transport => {
                        scope.foreign_transport = None;
                    }
                    None if scope.foreign_invocations.len() == 1 => {
                        scope.foreign_transport = Some(transport.clone());
                    }
                    _ => {}
                }
            }
            Op::Connect {
                target,
                request_digest,
            } => {
                scope.network = Some(target.clone());
                scope.http_request_digest = Some(request_digest.clone());
            }
            Op::SecretEgress { authority } => {
                scope.secret_egress = Some(authority.clone());
            }
            Op::ProtocolConnect { target } => {
                scope.network = Some(target.clone());
                scope.http_request_digest = None;
            }
        }
    }
    scope
}

/// Scope mechanics, and the one place `mint` can still be called.
///
/// These tests need private `PolicyEngine::mint`, so they stay inside this
/// module. They check independent path authorities, component-wise path
/// containment and guarded rechecks without exposing grant construction.
#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::time::Duration;

    use futures::executor::block_on;
    use futures::future::{self, BoxFuture};
    use serde_json::json;

    use super::*;

    mod permission_presentation_red {
        use super::*;
        use crate::config::rule_management::OmittedDimension;
        use crate::permit::analysis::{RiskClass, RiskReason};
        use crate::permit::{HttpRequest, HttpRequestDigest};

        // Catches: workspace labels inferred from the tool kind or a string prefix.
        #[test]
        fn write_risk_uses_canonical_path_not_tool_name() {
            let engine =
                PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());
            for (tool, path, expected) in [
                ("write", "/elsewhere/result.md", "outside_workspace_write"),
                ("edit", "/ws-other/result.md", "outside_workspace_write"),
                ("custom", "/ws/result.md", "workspace_write"),
            ] {
                let call = PreparedCall::new(
                    tool,
                    Op::Write {
                        path: CanonPath::new(path),
                        change: Change::Whole(b"hello".to_vec()),
                    },
                );
                let risk = engine.risk(&call);
                assert_eq!(risk.class, RiskClass::Bounded);
                assert_eq!(
                    serde_json::to_value(&risk.reasons).unwrap(),
                    json!([expected]),
                    "{tool} {path}"
                );
            }
        }

        #[test]
        fn pending_permission_retains_exact_input_risk_and_canonical_rule_facts() {
            let params = json!({
                "path": "notes.txt",
                "contents": "hello",
                "metadata": { "source": "model" }
            });
            let schema = json!({
                "type": "object",
                "required": ["path", "contents"],
                "properties": {
                    "path": { "type": "string" },
                    "contents": { "type": "string" },
                    "metadata": { "type": "object" }
                }
            });
            let call = PreparedCall::new(
                "write",
                Op::Write {
                    path: CanonPath::new("/ws/notes.txt"),
                    change: Change::Whole(b"hello".to_vec()),
                },
            );
            let engine =
                PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());

            let Classification::Ask(pending) = engine.classify_presented(
                call,
                PermissionPresentationInput::new(params.clone(), schema.clone()),
            ) else {
                panic!("an uncovered write in default mode must ask");
            };
            let Resolution::Ask(pending) = block_on(engine.settle(pending)) else {
                panic!("default mode must preserve the question for the human");
            };
            let presentation = pending
                .request()
                .presentation()
                .expect("effect-tool questions carry a presentation");

            assert_eq!(presentation.parameters(), &params);
            assert_eq!(presentation.schema(), &schema);
            assert_eq!(presentation.risk().class, RiskClass::Bounded);
            assert_eq!(
                presentation.risk().reasons,
                vec![RiskReason::WorkspaceWrite]
            );
            let plan = presentation
                .remember_plan()
                .expect("a workspace write is representable");
            assert_eq!(plan.candidates.len(), 1);
            assert!(plan.candidates[0].to_string().contains("/ws/notes.txt"));
            assert_eq!(plan.omitted, vec![OmittedDimension::Change]);
        }

        #[test]
        fn network_permission_exposes_its_canonical_widening_fact() {
            let target = NetworkTarget::new("example.com", 443).unwrap();
            let call = PreparedCall::new(
                "http",
                Op::Connect {
                    target,
                    request_digest: HttpRequestDigest::of(&HttpRequest::get("/").unwrap()),
                },
            );
            let engine =
                PolicyEngine::new(CanonPath::new("/ws"), Mode::Default, Ruleset::default());

            let Classification::Ask(pending) = engine.classify_presented(
                call,
                PermissionPresentationInput::new(
                    json!({ "url": "https://example.com" }),
                    json!({ "type": "object" }),
                ),
            ) else {
                panic!("an uncovered connection in default mode must ask");
            };

            let plan = pending
                .request()
                .presentation()
                .unwrap()
                .remember_plan()
                .expect("a literal endpoint is representable");
            assert_eq!(plan.candidates.len(), 1);
            assert_eq!(plan.candidates[0].to_string(), "net(example.com:443)");
            assert_eq!(plan.omitted, vec![OmittedDimension::RequestDigest]);
        }
    }

    #[test]
    fn analysis_model_cannot_lower_high_risk_or_create_authority_outside_auto() {
        use crate::permit::analysis::{RiskClass, RiskReason, assess};
        use crate::permit::judge::JudgeOutcome;
        let target = NetworkTarget::new("example.com", 443).unwrap();
        let call = PreparedCall::hook_http(
            crate::event::hook::HookId::parse(&format!("h1:{}", "11".repeat(32))).unwrap(),
            crate::permit::HttpRoute::new(crate::permit::HttpScheme::Https, target.clone())
                .unwrap(),
            &HttpRequest::get("/").unwrap(),
            vec!["credential".into()],
        );
        let risk = assess(&call);
        assert_eq!(risk.class, RiskClass::High);
        assert!(risk.reasons.contains(&RiskReason::SecretRelease));
        for mode in [Mode::Auto, Mode::Plan, Mode::Default] {
            let engine = PolicyEngine::new(CanonPath::new("/ws"), mode, Ruleset::default());
            let pending = engine.pending(
                call.clone(),
                Escalation::NoRuleCoversSubject {
                    op: OpName::SecretEgress,
                    subject: Subject::Network(target.clone()),
                },
            );
            assert!(matches!(
                engine.settle_judge(
                    pending,
                    &JudgeOutcome::Allow {
                        reason: "safe".into()
                    },
                    Authority::CachedJudge {
                        model: crate::config::ModelRef("provider/model".into()),
                        analysis: "11".repeat(32)
                    }
                ),
                Resolution::Ask(_)
            ));
        }
    }
    use crate::mcp::ToolExecution;
    use crate::permit::fake::{FakeFs, FakeWriteTool};
    use crate::permit::{
        Change, DnsResolution, DuplexHandle, DuplexSpawner, EnvPolicy, Fs, GuardedEffects,
        GuardedIo, HttpRequest, IoDenied, NetworkTarget, OneHop, OneHopResponse, Op, PrepareCtx,
        ProcessError, ProcessExecutor, ResolvedConnect, Resolver, Tool, ToolError, ToolResult,
        TransportError, TransportFailure, Typed,
    };
    use crate::store::blob::{BlobError, BlobPage, BlobRead, BlobRef, BlobStore, BlobWriter};
    use crate::store::evidence::{EvidenceError, EvidencePage, EvidenceStore, EvidenceWriter};

    struct NoProcess;

    impl ProcessExecutor for NoProcess {
        fn execute<'a>(
            &'a self,
            _spec: &'a SpawnSpec,
        ) -> BoxFuture<'a, Result<ToolResult, ProcessError>> {
            Box::pin(future::ready(Err(ProcessError::Invariant(
                "process execution is not used by filesystem tests".to_string(),
            ))))
        }
    }

    struct NoEvidence;

    impl BlobStore for NoEvidence {
        fn begin(&self) -> Result<Box<dyn BlobWriter>, BlobError> {
            Err(BlobError::Limit { limit: 0 })
        }

        fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, BlobError> {
            Err(BlobError::NotFound {
                reference: reference.id.clone(),
            })
        }

        fn read_page<'a>(
            &'a self,
            request: &'a BlobRead,
        ) -> BoxFuture<'a, Result<BlobPage, BlobError>> {
            Box::pin(future::ready(Err(BlobError::NotFound {
                reference: request.reference.id.clone(),
            })))
        }

        fn collect_garbage(&self, _reachable: &[BlobRef]) -> Result<usize, BlobError> {
            Ok(0)
        }
    }

    impl EvidenceStore for NoEvidence {
        fn blob_store(&self) -> &dyn BlobStore {
            self
        }

        fn begin(&self) -> Result<Box<dyn EvidenceWriter>, EvidenceError> {
            Err(EvidenceError::Limit { limit: 0 })
        }

        fn read_page<'a>(
            &'a self,
            request: &'a EvidenceRead,
        ) -> BoxFuture<'a, Result<EvidencePage, EvidenceError>> {
            Box::pin(future::ready(Err(EvidenceError::NotFound {
                reference: request.reference.id.clone(),
            })))
        }
    }

    struct NoForeign;

    impl Resolver for NoForeign {
        fn resolve(
            &self,
            _target: NetworkTarget,
            _deadline: std::time::Instant,
        ) -> BoxFuture<'static, Result<DnsResolution, TransportError>> {
            Box::pin(future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))))
        }
    }

    impl OneHop for NoForeign {
        fn begin_call(&self) -> std::time::Instant {
            std::time::Instant::now() + std::time::Duration::from_secs(60)
        }

        fn execute(
            &self,
            _connect: ResolvedConnect,
            _request: HttpRequest,
            _deadline: std::time::Instant,
        ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
            Box::pin(future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))))
        }
    }

    impl DuplexSpawner for NoForeign {
        fn spawn(
            &self,
            _spec: SpawnSpec,
        ) -> BoxFuture<'static, Result<DuplexHandle, TransportError>> {
            Box::pin(future::ready(Err(TransportError::definitely_unsent(
                TransportFailure::AuthorityMismatch,
            ))))
        }
    }

    static NO_PROCESS: NoProcess = NoProcess;
    static NO_EVIDENCE: NoEvidence = NoEvidence;
    fn guarded(permit: Permit, fs: &dyn Fs) -> GuardedEffects<'_> {
        GuardedEffects::new(
            permit,
            fs,
            &NO_PROCESS,
            &NO_EVIDENCE,
            Arc::new(NoForeign),
            Arc::new(NoForeign),
            Arc::new(NoForeign),
        )
    }

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
    /// `Approver` is a public port and must express refusal as well as approval.
    /// Its `Deny` result must remain reachable for external parties even when
    /// the mode approver does not produce it.
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
        PreparedCall::new(
            "edit",
            Op::Write {
                path: CanonPath::new(path),
                change: Change::Whole(b"whatever".to_vec()),
            },
        )
    }

    #[test]
    fn permission_request_renders_whole_content_exactly_and_safely() {
        let request = PermissionRequest {
            tool: "fake\u{1b}write".to_string(),
            ops: vec![Op::Write {
                path: CanonPath::new("/ws/out.txt"),
                change: Change::Whole(b"hello\n\x1b".to_vec()),
            }],
            why: Escalation::ModeAsks {
                mode: Mode::Default,
                op: OpName::Write,
                path: CanonPath::new("/ws/out.txt"),
            },
            presentation: None,
            child_name: None,
            explicit_answer: false,
        };

        assert_eq!(
            request.to_string(),
            concat!(
                "fake\\u{1b}write needs permission: default mode does not approve write ",
                "/ws/out.txt on its own\n",
                "- write /ws/out.txt\n",
                "  whole content (7 bytes): hello\\n\\x1b",
            )
        );
    }

    #[test]
    fn permission_request_renders_every_operation_and_conditional_replacement_exactly() {
        let request = PermissionRequest {
            tool: "tools".to_string(),
            ops: vec![
                Op::Read {
                    path: CanonPath::new("/ws/read.txt"),
                },
                Op::List {
                    root: CanonPath::new("/ws/src"),
                    pattern: "*.rs\n".to_string(),
                },
                Op::Search {
                    root: CanonPath::new("/ws/src"),
                    query: "unsafe\u{1b}".to_string(),
                },
                Op::Write {
                    path: CanonPath::new("/ws/edit.txt"),
                    change: Change::Replace {
                        replace_all: false,
                        old_string: "old\n".to_string(),
                        new_string: "new\t".to_string(),
                    },
                },
            ],
            why: Escalation::NoRuleCovers {
                op: OpName::Read,
                path: CanonPath::new("/ws/read.txt"),
            },
            presentation: None,
            child_name: None,
            explicit_answer: false,
        };

        assert_eq!(
            request.to_string(),
            concat!(
                "tools needs permission: no rule covers read /ws/read.txt\n",
                "- read /ws/read.txt\n",
                "- list /ws/src matching *.rs\\n\n",
                "- search /ws/src for unsafe\\u{1b}\n",
                "- write /ws/edit.txt\n",
                "  replace exactly one occurrence of old text: old\\n\n",
                "  with new text: new\\t\n",
                "  final content depends on the file at execution time",
            )
        );
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
        let guarded = guarded(permit, &fs);

        let result = block_on(tool.execute(call, &guarded)).unwrap();
        let ToolExecution::Complete(result) = result else {
            panic!("write must complete")
        };
        assert_eq!(result.model_text(), "wrote 5 bytes to /ws/allowed/note.txt");
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
        let guarded = guarded(permit, &fs);

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
            call.ops(),
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

        let guarded = guarded(permit, &fs);
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
        let guarded = guarded(permit, &fs);

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
        let guarded = guarded(permit, &fs);

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
        let guarded = guarded(permit, &fs);

        // The scope boundary itself is in scope ("is or under", not
        // strictly-under-only)...
        guarded
            .write(&CanonPath::new("/ws/allowed"), b"root")
            .unwrap();
        //...and so is a nested descendant, arbitrarily deep.
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
        let guarded = guarded(permit, &fs);

        // Same sibling-prefix hazard as the write case, checked against
        // `Scope::read_paths` — read and write scope independently, each
        // with its own `.iter().any()`, so this needs its own pin.
        let err = guarded
            .read(&CanonPath::new("/ws/allowed-evil/secret.txt"))
            .unwrap_err();
        assert!(matches!(err, IoDenied::Permit(PermitDenied::Read { .. })));
    }

    #[test]
    fn spawn_denial_preserves_exact_spec_and_diagnostic_when_payload_is_boxed() {
        let spec = SpawnSpec::new(
            vec![OsString::from("git"), OsString::from("status")],
            None,
            Some(Duration::from_secs(30)),
            EnvPolicy::Inherited,
        )
        .unwrap();
        let permit = minter().mint(Scope::default());
        let denied = permit.allows_spawn(&spec).unwrap_err();
        assert_eq!(
            denied.to_string(),
            format!("spawn denied: specification {spec:?} is outside the permit's scope")
        );
        let PermitDenied::Spawn { spec: denied_spec } = denied else {
            panic!("expected a spawn denial");
        };
        assert_eq!(*denied_spec, spec);
    }

    #[test]
    fn permit_scope_covers_paths_endpoints_and_exact_spawn_spec() {
        let spawn_cwd = if cfg!(windows) {
            CanonPath::new(r"C:\ws")
        } else {
            CanonPath::new("/ws")
        };
        let allowed_spawn = SpawnSpec::new(
            vec![OsString::from("git"), OsString::from("status")],
            Some(spawn_cwd.clone()),
            Some(Duration::from_secs(30)),
            EnvPolicy::Inherited,
        )
        .unwrap();
        let engine = minter();
        let permit = engine.mint(Scope {
            write_paths: vec![CanonPath::new("/ws/allowed")],
            read_paths: vec![CanonPath::new("/ws/readable")],
            list_paths: vec![CanonPath::new("/ws/listable")],
            search_paths: vec![CanonPath::new("/ws/searchable")],
            endpoints: vec!["api.example.com:443".to_string()],
            spawns: vec![allowed_spawn.clone()],
            ..Scope::default()
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

        assert!(permit.allows_spawn(&allowed_spawn).is_ok());
        let widened_spawn = SpawnSpec::new(
            vec![
                OsString::from("git"),
                OsString::from("status"),
                OsString::from("--short"),
            ],
            Some(spawn_cwd),
            Some(Duration::from_secs(30)),
            EnvPolicy::Inherited,
        )
        .unwrap();
        assert!(matches!(
            permit.allows_spawn(&widened_spawn),
            Err(PermitDenied::Spawn { .. })
        ));
    }
}
