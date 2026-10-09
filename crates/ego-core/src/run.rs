//! Why an *invocation* ended, as opposed to why a turn did.
//!
//! See `ARCHITECTURE.md` and `CLI.md` §4.3.
//!
//! Two levels rather than one enum, because a turn and a run are not the same
//! thing. `max_turns` fires *between* two completed turns: recording it as
//! the cause of the preceding `TurnCompleted` would make the log say
//! something false about a turn that ended normally. An argv error happens
//! before any turn exists at all.
//!
//! Not durable, and that is the dividing line: everything here is either a
//! fact about this process (`SIGINT`, a bad command line) or a limit the
//! controller applied between turns. A cause that interrupts a *running* turn
//! is durable and lives on [`ExitReason`] — see
//! [`CancelCause`](crate::event::domain::CancelCause).
//!
//! It lives in `ego-core` rather than in the binary so that the CLI and an
//! ACP host share one vocabulary instead of each inventing its own mapping to
//! the same table.

use crate::event::domain::{CancelCause, ExitReason};
use crate::permit::{Authority, Confinement, ExecObservation, HumanApprover, SandboxGrantPort};
use crate::public::QuestionReport;

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use futures::StreamExt;
use futures::future::BoxFuture;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::Digest;
use uuid::Uuid;

use crate::budget::SharedBudgetGate;
use crate::ceiling::Budget;
use crate::config::{Mode, ModelRef, Trust};
use crate::cost::Money;
pub use crate::event::id::SubagentInvocationId;
use crate::event::id::{ItemId, SessionId, TurnId};
use crate::event::time::{Clock, Timestamp};
use crate::evidence::{EvidenceError, EvidenceWrite, RunInvocationId};
use crate::generator::capabilities::ModelCapabilities;
use crate::generator::types::Image;
use crate::mcp::McpRuntimeBinding;
use crate::question::Elicitor;
use crate::session_lifecycle::{
    ActivityFuture, CloseFuture, ClosedSession, HoldSignalFuture, LifecycleError, SessionActivity,
    SessionRunLifecycle,
};
use crate::sink::EventSink;
use crate::store::session_store::{SessionEntry, SessionStore, StoreError};
use crate::turn::AdmittedToolSource;

/// Borrowed future returned by run adapters at controller-owned boundaries.
pub type RunFuture<'a, T> = BoxFuture<'a, T>;

impl SessionId {
    /// Raw UUID for exact rooted identity comparisons and protocol projection.
    pub fn uuid(&self) -> Uuid {
        Uuid::parse_str(&self.to_string()).expect("SessionId always formats as a UUID")
    }
}

/// Who refused the call that ended the run — or that nobody did.
///
/// The fifth case exit 3 could not express. `CLI.md` §4.4 offers three opt-ins
/// — `--allow`, a stronger `--sandbox`, `--yolo` — and says they "act on
/// different axes". Which of them applies is this value, and for one arm the
/// answer is *none of them*: a deny rule sends an operator to the config layer
/// `RuleAuthority::from` names, the mode floor sends them to `--mode`, the
/// boundary sends them to `--sandbox` and explicitly not to `--yolo`, and a
/// question nobody answered sends them to `--allow`.
///
/// [`Nobody`](Self::Nobody) rather than `Option<Authority>`, even though a
/// `None` here could only ever mean the one thing — `RunTermination` is not
/// durable, so nothing else can put one there. Every Rust reader's prior for
/// `Option` is "absent or unknown", and this is neither: it is a stated fact
/// about a run in which §6's order was exhausted. Same size, nothing to
/// re-derive. See
/// `ARCHITECTURE.md` §2.
///
/// There is deliberately no `Authority::Nobody` arm. That type is persisted
/// through `EventKind::PermissionDecided`, and a *decision* event whose
/// authority is "nobody decided" is a contradiction on disk that outlives
/// anyone who could explain it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeniedBy {
    /// A rule, the mode, the workspace boundary or a party at the
    /// [`Approver`](crate::permit::Approver) port. Built only from
    /// [`PolicyDenied::authority`](crate::permit::PolicyDenied::authority),
    /// which is the single definition of "who said so".
    Decided(Authority),
    /// Every party in the evaluation order was consulted and the question is
    /// still open. Nothing was decided, so there is nothing to name.
    Nobody,
}

/// What a permission-refused turn was refused, and by whom.
///
/// One value rather than two `Option` fields on [`TurnOutcome`], because they
/// cannot be true separately: there is no action without an
/// authority-or-nobody, and no authority without an action. Two Options would
/// be two things that must agree with nothing in the types saying so.
///
/// [`TurnOutcome`]: crate::turn::TurnOutcome
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Denial {
    /// What was refused, rendered for a person. `CLI.md` §4.4's "denied
    /// action".
    pub action: String,
    pub by: DeniedBy,
}

/// Why an invocation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunTermination {
    /// Every requested turn ran to completion.
    Done,
    /// The agent reports it could not do the task, or a `--schema` result
    /// never validated. Distinct from [`Failed`](Self::Failed): the run
    /// worked, the task did not.
    TaskFailed,
    /// The command line itself was wrong. No turn ever ran.
    UsageError { message: String },
    /// A call needed a grant this run could not produce. Carries the action so
    /// §4.1's result can name it — a script that gets exit 3 and no subject
    /// cannot act on it — and [`DeniedBy`] so it can tell a refusal somebody
    /// made from a question nobody answered, which want different remedies.
    ///
    /// Two fields and not two variants. The precedent that looks like it
    /// argues for a `PermissionUnanswered` beside this one is [`MaxTurns`]'s
    /// doc below, and it argues the other way: that split exists because §4.1
    /// spells the two limits as two different words, and §4.1 has exactly
    /// **one** word for permission. A second variant would return
    /// `"permission"` from [`exit_reason`](Self::exit_reason) like this one,
    /// exit 3 like this one, and still need a field to say anything.
    ///
    /// [`MaxTurns`]: Self::MaxTurns
    PermissionRequired { action: String, by: DeniedBy },
    /// A durable question awaits an answer from the invoking human.
    QuestionRequired { question: Box<QuestionReport> },
    /// The provider or the network failed, and the turn's effect is known:
    /// the failed attempt showed nothing, or the provider itself reported the
    /// failure. The run is settled and the session takes the next prompt.
    /// `message` names the cause once, in words a host may show.
    Provider { message: String },
    /// The exchange was lost after the turn had already shown output, so
    /// nobody can say what the provider did with the rest. The only provider
    /// failure that leaves the run uncertain.
    ProviderUncertain { message: String },
    /// The provider explicitly reported that the model context window was
    /// exhausted. The run is settled; a host may offer explicit compaction.
    ContextOverflow { message: String },
    /// Our own defect.
    Failed { message: String },
    /// A budget ceiling stopped the run.
    BudgetExceeded,
    /// `--max-turns` stopped the run. Two variants rather than one
    /// `LimitReached`, even though `CLI.md` §4.3 gives both exit 5, because
    /// §4.1 spells them as two different words (`budget` and `max_turns`).
    /// Fusing them here would make one of those words unproducible, and a
    /// ratified word nothing can emit is a contract we only claim to meet.
    /// They differ in kind too: a budget ceiling interrupts a turn, and a
    /// turn limit is only ever reached *between* turns.
    MaxTurns,
    /// A host asked for a safe-boundary pause and the boundary was reached.
    ///
    /// Unreachable from a headless invocation and that is not an accident: a
    /// run with no connected host has nothing to ask for a hold, and nothing
    /// to later release it. It has an exit code and a word anyway, because a
    /// termination that cannot say what it is would be reported as our own
    /// defect the first time an embedding produced one.
    Paused,
    /// A deadline expired.
    TimedOut,
    /// A signal ended the process.
    Signalled(Signal),
}

/// The signals with a defined exit code in `CLI.md` §4.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Int,
    Term,
}

impl RunTermination {
    /// The process exit code, per `CLI.md` §4.3.
    ///
    /// The mapping lives here and not in the binary because it is the same
    /// mapping for every front end, and a second copy in an ACP host would be
    /// a second chance to disagree about what "budget" exits with.
    pub fn exit_code(&self) -> u8 {
        match self {
            RunTermination::Done => 0,
            RunTermination::TaskFailed => 1,
            RunTermination::Failed { .. } => 1,
            RunTermination::UsageError { .. } => 2,
            RunTermination::PermissionRequired { .. } => 3,
            RunTermination::QuestionRequired { .. } => 3,
            RunTermination::Provider { .. } | RunTermination::ProviderUncertain { .. } => 4,
            RunTermination::ContextOverflow { .. } => 4,
            RunTermination::BudgetExceeded => 5,
            RunTermination::MaxTurns => 5,
            // A code of its own and not 3, though both stop for a human. §4.1
            // ratifies that on exit 3 "exactly one human-input payload is
            // present" — a `denied`/`denied_by` pair or a `question` object —
            // and a pause has neither: nothing was refused and nothing is
            // being asked. A script that reads 3 goes looking for a payload
            // that is not there. 6 is the first free code in §4.3's table and
            // no script can be observing it today.
            RunTermination::Paused => 6,
            RunTermination::TimedOut => 124,
            RunTermination::Signalled(Signal::Int) => 130,
            RunTermination::Signalled(Signal::Term) => 143,
        }
    }

    /// The `exit_reason` string of `CLI.md` §4.1.
    ///
    /// Neither finer nor coarser than [`exit_code`](Self::exit_code) — just
    /// cut differently. §4.1 publishes six words and §4.3 nine codes, and the
    /// two are separately ratified contracts rather than two spellings of
    /// one: `error` covers five codes, while one code (5) covers two words.
    /// A caller that needs a distinction the other side makes reads both.
    pub fn exit_reason(&self) -> &'static str {
        match self {
            RunTermination::Done => "done",
            RunTermination::BudgetExceeded => "budget",
            RunTermination::MaxTurns => "max_turns",
            RunTermination::TimedOut => "timeout",
            RunTermination::PermissionRequired { .. } => "permission",
            RunTermination::QuestionRequired { .. } => "question",
            RunTermination::Paused => "paused",
            RunTermination::TaskFailed
            | RunTermination::UsageError { .. }
            | RunTermination::Provider { .. }
            | RunTermination::ProviderUncertain { .. }
            | RunTermination::ContextOverflow { .. }
            | RunTermination::Failed { .. }
            | RunTermination::Signalled(_) => "error",
        }
    }

    /// The core-owned completion fragment used by headless hosts. Rich CLI
    /// accounting remains an adapter concern; the question itself does not.
    pub fn report(&self) -> RunReport {
        RunReport {
            ok: matches!(self, Self::Done),
            exit_reason: self.exit_reason(),
            question: match self {
                Self::QuestionRequired { question } => Some(question.as_ref().clone()),
                _ => None,
            },
        }
    }

    /// How a run ends when its last turn ended for `reason`.
    ///
    /// The turn level answers for itself wherever it can, so a front end
    /// never re-derives it. `Completed` returns `None`: a completed turn does
    /// not end a run — the controller decides whether another turn follows,
    /// and only it knows. That is also why no turn can produce
    /// [`MaxTurns`](Self::MaxTurns): it is a fact about the invocation that no
    /// single turn is in a position to report.
    ///
    /// **[`UsageError`](Self::UsageError) was in that sentence and should not
    /// have been.** A turn is in fact the only thing positioned to notice one
    /// class of it: the invocation and the *resolved* model disagree — a
    /// `--thinking` level the model does not accept — and nothing above the
    /// turn holds the resolved [`ModelCapabilities`] to compare against. See
    /// `ARCHITECTURE.md`.
    ///
    /// [`ModelCapabilities`]: crate::generator::capabilities::ModelCapabilities
    ///
    /// `denied` is what a [`ExitReason::PermissionRequired`] turn was refused
    /// and who refused it, which `CLI.md` §4.1 and §4.4 require in the result.
    /// A `PermissionRequired` turn that recorded no denial is reported as our
    /// own defect rather than as exit 3 with an empty subject: a script that
    /// gets 3 and cannot see what was denied has been told nothing, and
    /// inventing a placeholder would hide the bug that produced it. The
    /// missing thing is the whole [`Denial`], so an action can no longer
    /// arrive without an authority beside it.
    /// [`after_turn`](Self::after_turn) for a whole turn outcome.
    ///
    /// A provider failure carries what the turn learned about it: the cause a
    /// host may be shown, and whether the turn's effect is known.
    /// Usage failures carry the current turn's journal diagnostic. Other
    /// outcomes are decided by the exit reason alone.
    pub fn after_outcome(outcome: &crate::turn::TurnOutcome) -> Option<Self> {
        if outcome.exit_reason == ExitReason::Usage
            && let Some(message) = &outcome.usage_error
        {
            return Some(Self::UsageError {
                message: message.clone(),
            });
        }
        match (&outcome.exit_reason, &outcome.provider_failure) {
            (ExitReason::Provider, Some(failure)) if failure.effect_known => {
                Some(RunTermination::Provider {
                    message: failure.cause.clone(),
                })
            }
            (ExitReason::Provider, Some(failure)) => Some(RunTermination::ProviderUncertain {
                message: failure.cause.clone(),
            }),
            _ => Self::after_turn(outcome.exit_reason, outcome.denied.clone()),
        }
    }

    pub fn after_turn(reason: ExitReason, denied: Option<Denial>) -> Option<Self> {
        match reason {
            ExitReason::Completed => None,
            ExitReason::Budget => Some(RunTermination::BudgetExceeded),
            ExitReason::Paused => Some(RunTermination::Paused),
            ExitReason::PermissionRequired => Some(match denied {
                Some(Denial { action, by }) => RunTermination::PermissionRequired { action, by },
                None => RunTermination::Failed {
                    message: "a turn ended needing a permission but recorded no action".to_string(),
                },
            }),
            ExitReason::Cancelled(CancelCause::Timeout) => Some(RunTermination::TimedOut),
            ExitReason::Cancelled(CancelCause::User) => {
                Some(RunTermination::Signalled(Signal::Int))
            }
            ExitReason::Cancelled(CancelCause::Host) => {
                Some(RunTermination::Signalled(Signal::Term))
            }
            ExitReason::Provider => Some(RunTermination::Provider {
                message: "the provider or the network failed".to_string(),
            }),
            ExitReason::ContextOverflow => Some(RunTermination::ContextOverflow {
                message: "the provider context window is exhausted".to_string(),
            }),
            // A summary line, like `Provider` above and for the same reason:
            // the gaps themselves are already in the log as an
            // `EventKind::Error`, which is what `--output json` streams. This
            // is what a caller reads when it reads nothing else.
            ExitReason::Usage => Some(RunTermination::UsageError {
                message: "the model does not accept the request this run asks for".to_string(),
            }),
            ExitReason::Internal => Some(RunTermination::Failed {
                message: "the turn failed for an internal reason".to_string(),
            }),
        }
    }
}

/// Stable structured termination information owned by core.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RunReport {
    pub ok: bool,
    pub exit_reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<QuestionReport>,
}

/// Stable identity supplied by an inbound protocol for one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(Uuid);

impl RequestId {
    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub const fn uuid(self) -> Uuid {
        self.0
    }

    /// The authoritative session target for a start command.
    pub fn session_id(request: Self) -> SessionId {
        // UUIDv5 is SHA-1(namespace || name), with its version and variant
        // bits normalised after hashing. Keeping the small digest here avoids
        // adding a second identity dependency to the domain crate.
        const RUN_START_NAMESPACE: [u8; 16] = [
            0xa4, 0x77, 0xc8, 0x57, 0x54, 0xca, 0x5d, 0x64, 0xa6, 0x4f, 0xce, 0x97, 0x3b, 0x95,
            0x69, 0x64,
        ];
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(&RUN_START_NAMESPACE);
        bytes[16..].copy_from_slice(request.0.as_bytes());
        let digest = sha1(&bytes);
        let mut uuid = [0_u8; 16];
        uuid.copy_from_slice(&digest[..16]);
        uuid[6] = (uuid[6] & 0x0f) | 0x50;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        Uuid::from_bytes(uuid)
            .to_string()
            .parse()
            .expect("a formatted UUID is a session id")
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for RequestId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// Digest of the complete inbound command body used for idempotency.
///
/// **Derived from the body, never a copy of it.** This value is written into
/// the durable run transaction (`RunCommand::CommandAdmitted`), which the
/// session store serialises to its JSONL, so a caller that passes the request
/// body itself persists that body a second time. For a text prompt that is
/// waste; for a prompt that can carry image blocks it breaks
/// `docs/decisions/2026-08-29-images-are-session-blobs-before-they-are-wire-
/// parts.md`, which rules that durable prompt records carry a reference, a
/// media type and dimensions and "never carry bytes".
///
/// [`Self::of`] is how a protocol adapter should build one. [`Self::parse`]
/// stays for the callers whose identity is already derived and structured —
/// `subagent.rs`'s `child:<invocation>:<sha256>` names its origin as well as
/// its content — so the rule is bounded derivation, not one fixed spelling.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestDigest(String);

impl RequestDigest {
    pub fn parse(value: &str) -> Result<Self, RunError> {
        if value.is_empty() {
            return Err(RunError::InvalidRequestDigest);
        }
        Ok(Self(value.to_owned()))
    }

    /// The digest of one serializable inbound command body.
    ///
    /// One implementation for every protocol adapter, so two of them cannot
    /// disagree about what makes a retry the same request. Serialization of a
    /// request this crate's callers own is infallible; a body that somehow
    /// cannot be serialized has no identity to compare, and is reported as the
    /// same invalid digest an empty string would be.
    pub fn of(body: &impl Serialize) -> Result<Self, RunError> {
        let bytes = serde_json::to_vec(body).map_err(|_| RunError::InvalidRequestDigest)?;
        Self::parse(&format!("{:x}", sha2::Sha256::digest(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated, immutable native workspace root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunRoot(PathBuf);

impl RunRoot {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, RunError> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(RunError::InvalidRoot { path });
        }
        Ok(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Directly routable run identity: durable session plus independent nonce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RunId {
    session_id: SessionId,
    nonce: Uuid,
}

impl RunId {
    /// Build a run identity from its already-typed session and independent nonce.
    pub const fn new(session_id: SessionId, nonce: Uuid) -> Self {
        Self { session_id, nonce }
    }

    pub const fn session_id(self) -> SessionId {
        self.session_id
    }

    pub const fn nonce(self) -> Uuid {
        self.nonce
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIdParseError(String);

impl std::fmt::Display for RunIdParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid run id `{}`", self.0)
    }
}

impl std::error::Error for RunIdParseError {}

impl std::str::FromStr for RunId {
    type Err = RunIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (session, nonce) = value
            .split_once('.')
            .ok_or_else(|| RunIdParseError(value.to_owned()))?;
        if nonce.contains('.') {
            return Err(RunIdParseError(value.to_owned()));
        }
        Ok(Self {
            session_id: session
                .parse()
                .map_err(|_| RunIdParseError(value.to_owned()))?,
            nonce: Uuid::parse_str(nonce).map_err(|_| RunIdParseError(value.to_owned()))?,
        })
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.session_id, self.nonce)
    }
}

/// Durable causal origin shared by every transaction in one rooted run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunOrigin {
    #[default]
    TopLevel,
    Subagent {
        invocation: SubagentInvocationId,
        parent_run: RunId,
        parent_turn: TurnId,
        parent_item: ItemId,
    },
}

/// Monotonic revision of the lifecycle records in one run stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunRevision(u64);

impl RunRevision {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn next(self) -> Self {
        Self(self.0 + 1)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Immutable caller selections carried through admission and durable replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSelections {
    model: Option<ModelRef>,
    thinking: Option<crate::generator::types::ThinkingLevel>,
    mode: Option<Mode>,
    #[serde(
        default,
        serialize_with = "serialize_run_budget",
        deserialize_with = "deserialize_run_budget"
    )]
    budget: Option<Budget>,
    profile: Option<String>,
    /// Optional ACP host profile ceiling, retained with durable caller selections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ceiling_profile: Option<String>,
    /// The confinement the caller picked, if they picked one.
    ///
    /// Here rather than in the per-generation seat because confinement is
    /// admitted once and everything downstream is derived from it — the MCP
    /// transports admitted for this run, the effect scope opened for it, and
    /// the child authority ceiling. A selection carried here lands before
    /// `RunConfinement` is built; one applied later would answer the caller
    /// `sandbox: ro` and run at something else.
    #[serde(default)]
    sandbox: Option<Confinement>,
    /// The measured process fact admitted with an ACP sandbox selection.
    ///
    /// This crosses the existing durable run boundary because the executor
    /// must compare effect construction with the exact session observation;
    /// keeping it in an adapter-side registry would create a second owner for
    /// admission truth. Non-ACP runs have no session observation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox_observation: Option<ExecObservation>,
    /// The workspace posture this run is being started at, if anyone decided.
    ///
    /// Two callers fill it and they are answering the same question. A seat
    /// reports what a person answered for this workspace. A parent spawning a
    /// subagent reports the posture that child runs at — its own by default,
    /// `Untrusted` when the role or the call spent it bare — because a child
    /// is subject to the rules its parent is subject to unless the parent
    /// says otherwise. Neither can exceed what the parent already holds.
    ///
    /// Here for the same reason [`Self::sandbox`] is: trust is resolved once,
    /// at preparation, and the config layers, the instruction file, the
    /// project MCP servers, the skills and the hooks are all derived from it.
    /// A posture selected later would answer a question whose consequences
    /// have already been decided.
    ///
    /// `None` is not `Untrusted` with a different name: it means nobody was
    /// in a position to decide — no seat asked and no parent spawning. It is
    /// every plain headless caller. Preparation resolves it to a bare posture
    /// all the same, but only a value here is somebody's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    trust: Option<Trust>,
}

impl RunSelections {
    pub fn new(
        model: Option<ModelRef>,
        thinking: Option<crate::generator::types::ThinkingLevel>,
        mode: Option<Mode>,
        budget: Option<Budget>,
        profile: Option<String>,
    ) -> Self {
        Self {
            model,
            thinking,
            mode,
            budget,
            profile,
            ceiling_profile: None,
            sandbox: None,
            sandbox_observation: None,
            trust: None,
        }
    }

    /// The confinement this run is pinned to.
    ///
    /// A builder rather than a seventh positional argument: every existing
    /// caller selects no confinement, and making all of them say `None` again
    /// would be six edits that mean nothing.
    #[must_use]
    pub const fn with_sandbox(mut self, sandbox: Confinement) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    /// Pin a sandbox and the session measurement that admitted it together.
    #[must_use]
    pub fn with_sandbox_observation(
        mut self,
        sandbox: Confinement,
        observation: ExecObservation,
    ) -> Self {
        self.sandbox = Some(sandbox);
        self.sandbox_observation = Some(observation);
        self
    }

    pub const fn empty() -> Self {
        Self {
            model: None,
            thinking: None,
            mode: None,
            budget: None,
            profile: None,
            ceiling_profile: None,
            sandbox: None,
            sandbox_observation: None,
            trust: None,
        }
    }

    pub const fn sandbox(&self) -> Option<Confinement> {
        self.sandbox
    }

    /// The immutable process observation admitted with [`Self::sandbox`].
    pub const fn sandbox_observation(&self) -> Option<&ExecObservation> {
        self.sandbox_observation.as_ref()
    }

    /// Pin the posture a seat answered for this session.
    ///
    /// A builder rather than another positional argument, for the reason
    /// [`Self::with_sandbox`] gives: every existing caller answers nothing.
    #[must_use]
    pub const fn with_trust(mut self, trust: Trust) -> Self {
        self.trust = Some(trust);
        self
    }

    /// The posture a seat answered, or `None` when no seat was asked.
    pub const fn trust(&self) -> Option<Trust> {
        self.trust
    }

    pub fn model(&self) -> Option<&ModelRef> {
        self.model.as_ref()
    }

    pub const fn thinking(&self) -> Option<crate::generator::types::ThinkingLevel> {
        self.thinking
    }

    pub const fn mode(&self) -> Option<Mode> {
        self.mode
    }

    pub fn budget(&self) -> Option<&Budget> {
        self.budget.as_ref()
    }

    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    /// Pin a profile name already resolved and admitted by the config owner.
    pub fn with_profile_name(&self, profile: impl Into<String>) -> Self {
        let mut pinned = self.clone();
        pinned.profile = Some(profile.into());
        pinned
    }

    /// Pin the independent host ceiling alongside the selected profile.
    pub fn with_ceiling_profile(mut self, profile: impl Into<String>) -> Self {
        self.ceiling_profile = Some(profile.into());
        self
    }

    pub fn ceiling_profile(&self) -> Option<&str> {
        self.ceiling_profile.as_deref()
    }

    fn with_model(&self, model: ModelRef) -> Self {
        let mut pinned = self.clone();
        pinned.model = Some(model);
        pinned
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum RunBudgetCodec {
    Money(Money),
    Tokens(u64),
}

fn serialize_run_budget<S>(budget: &Option<Budget>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    budget
        .as_ref()
        .map(|budget| match budget {
            Budget::Money(money) => RunBudgetCodec::Money(*money),
            Budget::Tokens(tokens) => RunBudgetCodec::Tokens(*tokens),
        })
        .serialize(serializer)
}

fn deserialize_run_budget<'de, D>(deserializer: D) -> Result<Option<Budget>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(
        Option::<RunBudgetCodec>::deserialize(deserializer)?.map(|budget| match budget {
            RunBudgetCodec::Money(money) => Budget::Money(money),
            RunBudgetCodec::Tokens(tokens) => Budget::Tokens(tokens),
        }),
    )
}

/// Public durable lifecycle state. `Running` means admitted, not inferred
/// provider activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Cancelling,
    Settled,
    Paused,
    Cancelled,
    Interrupted,
    Uncertain,
}

/// Typed reason a submitted turn settled by cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    User,
    Host,
    Timeout,
}

/// Executor evidence recorded by the settlement transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum RunSettlement {
    Completed,
    ContextOverflow,
    /// The turn reached a safe boundary because a host asked it to. Settled
    /// as far as the lease is concerned — nothing is running and the lease is
    /// free — while the session itself stays held until `_ego/resume`.
    ///
    /// The hold lives on `SessionState::pause` and not on this status, so
    /// there is one authority for "may this session start work" instead of
    /// two that can disagree. It also leaves the resume able to take an
    /// ordinary lease rather than needing a bypass for the state it is
    /// trying to leave.
    Paused,
    Cancelled {
        reason: CancelReason,
    },
    PermissionRequired {
        action: String,
        by: DeniedBy,
    },
    /// A cancellation that arrived before the executor started the turn.
    ///
    /// Split out of [`Interrupted`], which used to carry both this and an
    /// internal invariant failure. That conflation was the defect: the two
    /// settle identically as far as the lease is concerned — nothing ran —
    /// but they are opposite answers to the only question a host asks about a
    /// finished turn. One is the turn doing what it was told; the other is
    /// this agent failing. `prompt_result` now answers `cancelled` for this
    /// and an error for that.
    ///
    /// It carries no [`CancelReason`] on purpose. `RunCommand::CancelRequested`
    /// does not record who asked, so naming a reason here would be inventing
    /// one. The distinction the surfaces need is failure-versus-cancellation,
    /// and that is exactly what this variant encodes and no more.
    CancelledBeforeStart,
    /// A known failure prevented a successful turn. This includes an internal
    /// invariant failure or a configuration refusal during preflight. It is
    /// never reported as a successful stop.
    ///
    /// The `diagnostic` is why. It is the sanitized [`RunError`] display the
    /// *original* caller was handed, kept here because that caller is the only
    /// one who ever saw it: the error was returned up the call stack and
    /// nothing durable recorded it. A retry of the same request replays this
    /// receipt and carries no execution, so without the diagnostic on the
    /// settlement itself the replay can only answer a generic sentence about a
    /// failure it cannot describe — a different answer to the same request.
    ///
    /// `Option` rather than `String` because a run settled by an older build
    /// wrote no diagnostic, and a record that omits it deserializes to `None`
    /// and answers exactly as it did before rather than failing to load.
    Interrupted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
    },
    /// This process cannot say whether the turn ran. Carries the same
    /// sanitized diagnostic, for the same reason, as [`Self::Interrupted`].
    Uncertain {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
    },
}

impl RunSettlement {
    pub const fn completed() -> Self {
        Self::Completed
    }

    pub const fn cancelled(reason: CancelReason) -> Self {
        Self::Cancelled { reason }
    }

    pub fn permission_required(action: impl Into<String>, by: DeniedBy) -> Self {
        Self::PermissionRequired {
            action: action.into(),
            by,
        }
    }

    fn status(&self) -> RunStatus {
        match self {
            Self::Completed | Self::ContextOverflow | Self::PermissionRequired { .. } => {
                RunStatus::Settled
            }
            Self::Paused => RunStatus::Paused,
            // A cancellation before start is a cancellation: the durable
            // status has to say so, or `session/list` shows an internal
            // failure where a host cancelled its own turn.
            Self::Cancelled { .. } | Self::CancelledBeforeStart => RunStatus::Cancelled,
            Self::Interrupted { .. } => RunStatus::Interrupted,
            Self::Uncertain { .. } => RunStatus::Uncertain,
        }
    }

    /// The sanitized reason this turn failed, if it failed and one was kept.
    ///
    /// Public because the inbound surfaces are what the diagnostic exists for:
    /// they answer a replayed request, and this is the only place the original
    /// answer survives.
    pub fn diagnostic(&self) -> Option<&str> {
        match self {
            Self::Interrupted { diagnostic } | Self::Uncertain { diagnostic } => {
                diagnostic.as_deref()
            }
            Self::Completed
            | Self::ContextOverflow
            | Self::Paused
            | Self::Cancelled { .. }
            | Self::PermissionRequired { .. }
            | Self::CancelledBeforeStart => None,
        }
    }
}

/// Which of the two failure settlements a failed execution records.
///
/// A separate type from [`RunSettlement`] so that a call site choosing how a
/// turn failed cannot pass a settlement that is not a failure, and so that the
/// diagnostic is built in one place — from the [`RunError`] itself — rather
/// than at each of the eight sites that settle one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutionFailure {
    /// An internal invariant failed. Nothing ran.
    Interrupted,
    /// This process cannot say whether the turn ran.
    Uncertain,
}

impl ExecutionFailure {
    fn settlement(self, source: &RunError) -> RunSettlement {
        let diagnostic = Some(source.to_string());
        match self {
            Self::Interrupted => RunSettlement::Interrupted { diagnostic },
            Self::Uncertain => RunSettlement::Uncertain { diagnostic },
        }
    }
}

/// Identity of one admitted turn. Only that lease may be cancelled or settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TurnLease(Uuid);

/// One immutable transition in the authoritative session stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunTransaction {
    revision: RunRevision,
    run_id: RunId,
    origin: RunOrigin,
    root: RunRoot,
    selections: RunSelections,
    command: RunCommand,
    status: RunStatus,
    observed_at: Timestamp,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pruned_receipts: Vec<TurnLease>,
}

impl RunTransaction {
    pub const fn revision(&self) -> RunRevision {
        self.revision
    }

    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn origin(&self) -> &RunOrigin {
        &self.origin
    }

    pub fn root(&self) -> &RunRoot {
        &self.root
    }

    pub fn selections(&self) -> &RunSelections {
        &self.selections
    }

    pub fn command(&self) -> &RunCommand {
        &self.command
    }

    pub const fn status(&self) -> RunStatus {
        self.status
    }

    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    pub fn pruned_receipts(&self) -> &[TurnLease] {
        &self.pruned_receipts
    }
}

/// The one transition whose facts are published by a run revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunCommand {
    CommandAdmitted {
        request_id: RequestId,
        digest: RequestDigest,
        expected_revision: Option<RunRevision>,
        lease: TurnLease,
        executor_id: Uuid,
        prompt: String,
        model: ModelRef,
        start: bool,
    },
    CancelRequested {
        request_id: RequestId,
        digest: RequestDigest,
        lease: TurnLease,
    },
    CommandSettled {
        lease: TurnLease,
        settlement: RunSettlement,
    },
}

/// Result of the store's atomic compare-and-append operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAppend {
    Committed { revision: RunRevision },
    Conflict { actual: RunRevision },
    CommittedDurabilityUncertain { diagnostic: String },
}

/// One prompt as it was submitted: its text, and the images sent beside it.
///
/// **Transient, and deliberately not the durable record.** The bytes here are
/// on their way to the session's content-addressed store; what survives in the
/// event stream is a `BlobRef`, a media type and dimensions, which is what
/// `docs/decisions/2026-08-29-images-are-session-blobs-before-they-are-wire-
/// parts.md` requires. `RunCommand::CommandAdmitted` therefore projects only
/// [`Self::text`], and this type is intentionally not `Serialize`: a durable
/// record that could hold it would be a durable record holding bytes.
///
/// [`Image`] is used rather than a looser inline type because it is already the
/// validated one — its constructor bounds the byte length and decodes the
/// dimensions *from the bytes*, so a declared media type the payload
/// contradicts fails to construct rather than being admitted and refused later.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunPrompt {
    text: String,
    images: Vec<Image>,
}

impl RunPrompt {
    pub fn new(text: impl Into<String>, images: Vec<Image>) -> Self {
        Self {
            text: text.into(),
            images,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn images(&self) -> &[Image] {
        &self.images
    }
}

/// A prompt that is only text, which is most of them.
impl<T: Into<String>> From<T> for RunPrompt {
    fn from(text: T) -> Self {
        Self::new(text, Vec::new())
    }
}

/// First prompt and immutable selections for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    session_id: SessionId,
    root: RunRoot,
    request_id: RequestId,
    digest: RequestDigest,
    selections: RunSelections,
    prompt: RunPrompt,
    origin: RunOrigin,
}

impl StartRequest {
    pub fn new(
        session_id: SessionId,
        root: RunRoot,
        request_id: RequestId,
        digest: RequestDigest,
        selections: RunSelections,
        prompt: impl Into<RunPrompt>,
    ) -> Self {
        Self {
            session_id,
            root,
            request_id,
            digest,
            selections,
            prompt: prompt.into(),
            origin: RunOrigin::TopLevel,
        }
    }

    pub fn with_origin(mut self, origin: RunOrigin) -> Self {
        self.origin = origin;
        self
    }

    pub fn origin(&self) -> &RunOrigin {
        &self.origin
    }
}

/// A later prompt, guarded by the revision observed by its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueRequest {
    run_id: RunId,
    request_id: RequestId,
    digest: RequestDigest,
    expected_revision: RunRevision,
    prompt: RunPrompt,
}

impl ContinueRequest {
    pub fn new(
        run_id: RunId,
        request_id: RequestId,
        digest: RequestDigest,
        expected_revision: RunRevision,
        prompt: impl Into<RunPrompt>,
    ) -> Self {
        Self {
            run_id,
            request_id,
            digest,
            expected_revision,
            prompt: prompt.into(),
        }
    }
}

/// Request-backed cancellation used by protocols which carry command identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRequest {
    run_id: RunId,
    request_id: RequestId,
    digest: RequestDigest,
    expected_revision: RunRevision,
}

impl CancelRequest {
    pub fn new(
        run_id: RunId,
        request_id: RequestId,
        digest: RequestDigest,
        expected_revision: RunRevision,
    ) -> Self {
        Self {
            run_id,
            request_id,
            digest,
            expected_revision,
        }
    }
}

/// Opaque cancellation authority minted with one admitted lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelKey {
    request: CancelRequest,
    lease: TurnLease,
}

/// Readiness request. Preparation must perform no writable run effect.
#[derive(Debug, Clone)]
pub struct RunPreparation {
    pub run_id: RunId,
    pub invocation: RunInvocationId,
    pub root: RunRoot,
    pub selections: RunSelections,
    pub continuation: bool,
    pub origin: RunOrigin,
}

pub struct RunPrepared<P> {
    model: ModelRef,
    capability: P,
}

impl<P> RunPrepared<P> {
    pub fn new(model: ModelRef, capability: P) -> Self {
        Self { model, capability }
    }

    pub fn model(&self) -> &ModelRef {
        &self.model
    }

    pub fn into_parts(self) -> (ModelRef, P) {
        (self.model, self.capability)
    }
}

/// Effect-free live-authority attachment for one admitted run.
#[derive(Debug, Clone)]
pub struct RunAttachment {
    pub run_id: RunId,
    pub root: RunRoot,
    pub model: ModelRef,
    pub origin: RunOrigin,
}

/// Opaque live identity held by the executor adapter.
pub struct RunHandle {
    run_id: RunId,
    authority: Arc<dyn Any + Send + Sync>,
}

impl RunHandle {
    pub fn new<A>(run_id: RunId, authority: A) -> Self
    where
        A: Any + Send + Sync,
    {
        Self {
            run_id,
            authority: Arc::new(authority),
        }
    }

    pub const fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn authority<A>(&self) -> Option<&A>
    where
        A: Any + Send + Sync,
    {
        self.authority.downcast_ref::<A>()
    }
}

impl Clone for RunHandle {
    fn clone(&self) -> Self {
        Self {
            run_id: self.run_id,
            authority: Arc::clone(&self.authority),
        }
    }
}

impl std::fmt::Debug for RunHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunHandle")
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

/// Factory for the live event projection selected after model resolution.
pub trait EventSinkFactory: Send + Sync {
    fn create(&self, capabilities: &ModelCapabilities) -> Arc<dyn EventSink>;
}

/// Non-serializable bindings that travel only with effect authority.
#[derive(Clone)]
pub struct RunExecutionBindings {
    pub sink_factory: Arc<dyn EventSinkFactory>,
    pub elicitor: Option<Arc<dyn Elicitor>>,
    admitted_tools: Option<Arc<dyn AdmittedToolSource>>,
    mcp_errors: Arc<[String]>,
    mcp_runtime: Option<Arc<dyn McpRuntimeBinding>>,
    budget_gate: Option<SharedBudgetGate>,
    effect_tools: Option<Arc<BTreeSet<String>>>,
    human: Option<Arc<dyn HumanApprover>>,
    sandbox_grants: Option<Arc<dyn SandboxGrantPort>>,
    controls: Option<Arc<dyn crate::control::SessionControls>>,
    host: Option<crate::turn::HostClient>,
    interactive_control: Option<Arc<Mutex<Option<crate::turn::InteractiveControl>>>>,
}

impl RunExecutionBindings {
    pub fn new(
        sink_factory: Arc<dyn EventSinkFactory>,
        elicitor: Option<Arc<dyn Elicitor>>,
    ) -> Self {
        Self {
            sink_factory,
            elicitor,
            admitted_tools: None,
            mcp_errors: Arc::from([]),
            mcp_runtime: None,
            budget_gate: None,
            effect_tools: None,
            human: None,
            sandbox_grants: None,
            controls: None,
            host: None,
            interactive_control: None,
        }
    }

    /// Transfer the sole turn coordinator's receiver to this execution.
    pub fn with_interactive_control(mut self, control: crate::turn::InteractiveControl) -> Self {
        self.interactive_control = Some(Arc::new(Mutex::new(Some(control))));
        self
    }

    pub fn take_interactive_control(&self) -> Option<crate::turn::InteractiveControl> {
        self.interactive_control.as_ref().and_then(|control| {
            control
                .lock()
                .expect("turn control binding lock poisoned")
                .take()
        })
    }

    /// Attach the client that initialized this execution's connection, so the
    /// model can be told which host it serves.
    pub fn with_host(mut self, host: crate::turn::HostClient) -> Self {
        self.host = Some(host);
        self
    }

    pub fn host(&self) -> Option<crate::turn::HostClient> {
        self.host.clone()
    }

    /// Attach the human permission seat this execution context owns.
    ///
    /// Sits beside `elicitor` for the same reason: both are the connection's
    /// answer to "who is the person here", and both belong to the execution
    /// rather than to the process. A headless run leaves it `None`, which is
    /// what makes an unresolved permission exit 3 instead of silently
    /// consulting a terminal that no operator is watching.
    pub fn with_human(mut self, human: Arc<dyn HumanApprover>) -> Self {
        self.human = Some(human);
        self
    }

    pub fn human(&self) -> Option<Arc<dyn HumanApprover>> {
        self.human.as_ref().map(Arc::clone)
    }

    /// Attach the seat that can widen this execution's process sandbox for one
    /// path. Travels beside `human` for the same reason: only a connected host
    /// has a person to ask, and a headless run keeps its sandbox as admitted.
    pub fn with_sandbox_grants(mut self, grants: Arc<dyn SandboxGrantPort>) -> Self {
        self.sandbox_grants = Some(grants);
        self
    }

    pub fn sandbox_grants(&self) -> Option<Arc<dyn SandboxGrantPort>> {
        self.sandbox_grants.as_ref().map(Arc::clone)
    }

    /// Attach the operator seat's generation controls for this execution.
    ///
    /// Travels beside `human` for the same reason: the connection, not the
    /// process, is what knows whether a person is sitting in front of a
    /// picker. A headless run leaves it `None` and keeps whatever the
    /// configuration resolved, which is why an unattended run cannot have its
    /// model changed out from under it by a window nobody is watching.
    pub fn with_controls(mut self, controls: Arc<dyn crate::control::SessionControls>) -> Self {
        self.controls = Some(controls);
        self
    }

    pub fn controls(&self) -> Option<Arc<dyn crate::control::SessionControls>> {
        self.controls.as_ref().map(Arc::clone)
    }

    /// Attach the live admitted MCP source owned by this execution context.
    pub fn with_mcp(
        mut self,
        admitted_tools: Arc<dyn AdmittedToolSource>,
        mcp_errors: Vec<String>,
        mcp_runtime: Arc<dyn McpRuntimeBinding>,
    ) -> Self {
        self.admitted_tools = Some(admitted_tools);
        self.mcp_errors = Arc::from(mcp_errors);
        self.mcp_runtime = Some(mcp_runtime);
        self
    }

    /// Attach sanitized MCP diagnostics when no server was admitted.
    pub fn with_mcp_errors(mut self, mcp_errors: Vec<String>) -> Self {
        self.mcp_errors = Arc::from(mcp_errors);
        self
    }

    /// Attach the root run family's one live budget authority.
    pub fn with_budget_gate(mut self, gate: SharedBudgetGate) -> Self {
        self.budget_gate = Some(gate);
        self
    }

    /// Attach the exact inherited effect-tool ceiling for a child execution.
    pub fn with_effect_tools(mut self, tools: BTreeSet<String>) -> Self {
        self.effect_tools = Some(Arc::new(tools));
        self
    }

    /// The exact shared gate used by generation-boundary executors.
    pub fn budget_gate(&self) -> Option<SharedBudgetGate> {
        self.budget_gate.clone()
    }

    pub fn effect_tools(&self) -> Option<Arc<BTreeSet<String>>> {
        self.effect_tools.as_ref().map(Arc::clone)
    }

    pub fn admitted_tools(&self) -> Option<Arc<dyn AdmittedToolSource>> {
        self.admitted_tools.as_ref().map(Arc::clone)
    }

    pub fn mcp_errors(&self) -> &[String] {
        &self.mcp_errors
    }

    pub fn mcp_runtime(&self) -> Option<Arc<dyn McpRuntimeBinding>> {
        self.mcp_runtime.as_ref().map(Arc::clone)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunExecutorError {
    message: String,
}

impl RunExecutorError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for RunExecutorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RunExecutorError {}

/// The sole external execution boundary owned by the controller consumer.
pub trait RunExecutor: Send + Sync {
    type Prepared: Any + Send + 'static;

    fn prepare<'a>(
        &'a self,
        request: &'a RunPreparation,
    ) -> BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>>;

    fn admit_run_context<'a>(
        &'a self,
        prepared: &'a Self::Prepared,
    ) -> RunFuture<'a, Result<EvidenceWrite, EvidenceError>>;

    fn attach<'a>(
        &'a self,
        request: &'a RunAttachment,
    ) -> BoxFuture<'a, Result<RunHandle, RunExecutorError>>;

    fn recover<'a>(
        &'a self,
        run: &'a RunId,
    ) -> BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>>;

    fn observe<'a>(
        &'a self,
        handle: &'a RunHandle,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>>;

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        prompt: &'a RunPrompt,
        prepared: Self::Prepared,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>>;

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>>;

    /// Ask the running turn to stop at its next boundary and hold there.
    ///
    /// Beside [`interrupt`](Self::interrupt) and never folded into it: a
    /// cancellation ends a run and a pause suspends one, and an executor
    /// that answered either for the other would discard work the host meant
    /// to keep or leave running what the host asked to stop.
    ///
    /// Carries the host's request id because the boundary event in the
    /// session log names it. That is what makes a repeated `_ego/pause`
    /// idempotent rather than a second, later hold.
    ///
    /// Returning `Ok` says the request was delivered, not that the boundary
    /// has been reached — only the log says that, and only once the turn has
    /// got there. See
    /// `ARCHITECTURE.md`.
    fn pause<'a>(
        &'a self,
        handle: &'a RunHandle,
        request: RequestId,
    ) -> BoxFuture<'a, Result<(), RunExecutorError>>;
}

#[derive(Debug, Clone)]
pub enum RecoveryEvidence {
    Running(RunHandle),
    Terminal,
    CancelledBeforeStart,
    Ambiguous,
    NotRecoverable,
}

/// Deterministic completed-receipt bound supplied by the deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunRetention {
    completed_receipts: NonZeroUsize,
}

impl RunRetention {
    pub const fn new(completed_receipts: NonZeroUsize) -> Result<Self, RunError> {
        Ok(Self { completed_receipts })
    }

    /// How many settled receipts a run keeps, and therefore how far back a
    /// replay can still be answered.
    ///
    /// Public because an inbound adapter that remembers which requests it has
    /// already answered must not remember further back than this. A remembered
    /// request whose receipt has been pruned would name a command the run can
    /// no longer replay, and the retry would be refused instead of re-answered
    /// — a worse outcome than not remembering it at all.
    pub const fn completed_receipts(&self) -> NonZeroUsize {
        self.completed_receipts
    }
}

#[derive(Debug)]
pub enum RunError {
    InvalidRequestDigest,
    InvalidRoot {
        path: PathBuf,
    },
    NotFound {
        run_id: RunId,
    },
    IdentityCollision {
        session_id: SessionId,
    },
    RequestIdMismatch {
        request_id: RequestId,
    },
    StaleRevision {
        expected: RunRevision,
        actual: RunRevision,
    },
    LeaseOccupied {
        run_id: RunId,
    },
    PublishedDurabilityUncertain {
        run_id: RunId,
        diagnostic: String,
    },
    Store(StoreError),
    Executor(RunExecutorError),
    Evidence {
        source: Box<EvidenceError>,
        settlement_diagnostic: Option<String>,
    },
    ExecutionSettlement {
        source: Box<RunError>,
        settlement_diagnostic: String,
    },
    Integrity {
        message: String,
    },
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequestDigest => f.write_str("request digest is empty"),
            Self::InvalidRoot { path } => write!(f, "run root is not absolute: {}", path.display()),
            Self::NotFound { run_id } => write!(f, "run {run_id} was not found"),
            Self::IdentityCollision { session_id } => {
                write!(f, "session target {session_id} is occupied")
            }
            Self::RequestIdMismatch { request_id } => write!(
                f,
                "request {} was reused with different content",
                request_id.0
            ),
            Self::StaleRevision { expected, actual } => write!(
                f,
                "run revision {} is stale; current revision is {}",
                expected.0, actual.0
            ),
            Self::LeaseOccupied { run_id } => write!(f, "run {run_id} already has an active turn"),
            Self::PublishedDurabilityUncertain { diagnostic, .. } => f.write_str(diagnostic),
            Self::Store(error) => error.fmt(f),
            Self::Executor(error) => error.fmt(f),
            Self::Evidence {
                source,
                settlement_diagnostic,
            } => {
                write!(f, "run evidence admission failed: {source}")?;
                if let Some(diagnostic) = settlement_diagnostic {
                    write!(f, "; settlement also failed: {diagnostic}")?;
                }
                Ok(())
            }
            Self::ExecutionSettlement {
                source,
                settlement_diagnostic,
            } => write!(
                f,
                "{source}; settlement also failed: {settlement_diagnostic}"
            ),
            Self::Integrity { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Evidence { source, .. } => Some(source.as_ref()),
            Self::ExecutionSettlement { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<StoreError> for RunError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl From<RunExecutorError> for RunError {
    fn from(value: RunExecutorError) -> Self {
        Self::Executor(value)
    }
}

/// Sanitized state reconstructed from the authoritative transaction stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSnapshot {
    run_id: RunId,
    origin: RunOrigin,
    root: RunRoot,
    selections: RunSelections,
    revision: RunRevision,
    status: RunStatus,
    settlement: Option<RunSettlement>,
}

impl RunSnapshot {
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }
    pub fn origin(&self) -> &RunOrigin {
        &self.origin
    }
    pub const fn session_id(&self) -> SessionId {
        self.run_id.session_id()
    }
    pub fn root(&self) -> RunRoot {
        self.root.clone()
    }
    pub fn selections(&self) -> &RunSelections {
        &self.selections
    }
    pub const fn revision(&self) -> RunRevision {
        self.revision
    }
    pub const fn status(&self) -> RunStatus {
        self.status
    }
    pub fn settlement(&self) -> Option<&RunSettlement> {
        self.settlement.as_ref()
    }
    pub const fn blocks_new_turn(&self) -> bool {
        matches!(
            self.status,
            RunStatus::Running | RunStatus::Cancelling | RunStatus::Uncertain
        )
    }
}

struct SubscriptionState {
    snapshot: RunSnapshot,
    complete: bool,
    wakers: Vec<Waker>,
}

/// Detachable observation of one admitted turn. Polling never drives execution.
pub struct RunSubscription {
    state: Arc<Mutex<SubscriptionState>>,
}

impl Clone for RunSubscription {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}

impl RunSubscription {
    pub fn snapshot(&self) -> RunSnapshot {
        self.state
            .lock()
            .expect("run subscription")
            .snapshot
            .clone()
    }
}

impl Future for RunSubscription {
    type Output = RunSnapshot;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.lock().expect("run subscription");
        if state.complete {
            Poll::Ready(state.snapshot.clone())
        } else {
            if !state.wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                state.wakers.push(cx.waker().clone());
            }
            Poll::Pending
        }
    }
}

pub struct RunObservation {
    snapshot: RunSnapshot,
    subscription: RunSubscription,
}

impl RunObservation {
    pub fn snapshot(&self) -> &RunSnapshot {
        &self.snapshot
    }
    pub fn subscription(&self) -> RunSubscription {
        self.subscription.clone()
    }
}

pub enum RunCommandOutcome {
    Admitted(RunAdmission),
    Replayed(RunObservation),
}

enum ExecutionMode {
    Submit {
        run_id: RunId,
        root: RunRoot,
        origin: RunOrigin,
        model: ModelRef,
        prompt: RunPrompt,
        prepared: Box<dyn Any + Send>,
    },
    RecoveredObserve {
        handle: RunHandle,
    },
}

/// Non-cloneable one-shot authority to perform exactly one admitted effect.
pub struct RunExecution {
    run_id: RunId,
    lease: TurnLease,
    bindings: RunExecutionBindings,
    mode: ExecutionMode,
    subscription: Arc<Mutex<SubscriptionState>>,
    liveness: Arc<ExecutionLiveness>,
}

pub struct RunAdmission {
    snapshot: RunSnapshot,
    cancel_key: CancelKey,
    subscription: RunSubscription,
    execution: Box<RunExecution>,
}

impl RunAdmission {
    pub const fn run_id(&self) -> RunId {
        self.snapshot.run_id()
    }
    pub const fn session_id(&self) -> SessionId {
        self.snapshot.session_id()
    }
    pub fn root(&self) -> RunRoot {
        self.snapshot.root()
    }
    pub const fn revision(&self) -> RunRevision {
        self.snapshot.revision()
    }
    /// The controller-minted authority for this admitted turn.
    pub const fn turn_lease(&self) -> TurnLease {
        self.cancel_key.lease
    }
    pub fn snapshot(&self) -> &RunSnapshot {
        &self.snapshot
    }
    pub fn cancel_key(&self) -> &CancelKey {
        &self.cancel_key
    }
    pub fn subscription(&self) -> RunSubscription {
        self.subscription.clone()
    }
    pub fn into_execution(self) -> RunExecution {
        *self.execution
    }
}

pub struct RunReattachment {
    snapshot: RunSnapshot,
    cancel_key: CancelKey,
    subscription: RunSubscription,
    execution: Box<RunExecution>,
}

impl RunReattachment {
    pub fn snapshot(&self) -> &RunSnapshot {
        &self.snapshot
    }
    pub fn cancel_key(&self) -> &CancelKey {
        &self.cancel_key
    }
    pub fn subscription(&self) -> RunSubscription {
        self.subscription.clone()
    }
    pub fn into_execution(self) -> RunExecution {
        *self.execution
    }
}

pub enum RunRecoveryOutcome {
    Reattached(RunReattachment),
    Observed(RunObservation),
}

pub trait IdSource: Send + Sync {
    fn next_uuid(&self) -> Uuid;
}

struct FoldedRun {
    snapshot: RunSnapshot,
    admissions: Vec<AdmissionReceipt>,
    cancels: BTreeMap<RequestId, (RequestDigest, TurnLease)>,
    active: Option<ActiveLease>,
}

#[derive(Clone)]
struct AdmissionReceipt {
    request_id: RequestId,
    digest: RequestDigest,
    lease: TurnLease,
    settlement: Option<RunSettlement>,
    /// What the run looked like at the moment *this* receipt settled.
    ///
    /// Without it a replay answers from `folded.snapshot`, which is the run as
    /// it stands now — so retrying a retained older receipt reports a later
    /// turn's revision and settlement as if they were its own. `None` while
    /// the turn is still open, which is the only time the current snapshot and
    /// the receipt's own state are the same thing.
    settled: Option<SettledOutcome>,
    start: bool,
    model: ModelRef,
    pruned: bool,
}

/// The terminal state one settlement produced, kept beside the receipt that
/// caused it. Both fields or neither: a settlement always advances the
/// revision and the status together.
#[derive(Clone, Copy)]
struct SettledOutcome {
    revision: RunRevision,
    status: RunStatus,
}

#[derive(Clone, Copy)]
struct ActiveLease {
    lease: TurnLease,
}

struct LiveState {
    handles: HashMap<RunId, RunHandle>,
    subscriptions: HashMap<(RunId, TurnLease), Arc<Mutex<SubscriptionState>>>,
    executions: HashMap<(RunId, TurnLease), Weak<ExecutionLiveness>>,
    interrupted: BTreeSet<(RunId, TurnLease)>,
}

struct ExecutionLiveness;

impl LiveState {
    fn new() -> Self {
        Self {
            handles: HashMap::new(),
            subscriptions: HashMap::new(),
            executions: HashMap::new(),
            interrupted: BTreeSet::new(),
        }
    }
}

/// The sole rooted lifecycle owner. Durable truth remains in `SessionStore`;
/// this type retains only live executor and observer capabilities.
pub struct RunController<S, E, C, I> {
    store: Arc<S>,
    executor: Arc<E>,
    clock: Arc<C>,
    ids: Arc<I>,
    retention: RunRetention,
    live: Mutex<LiveState>,
}

impl<S, E, C, I> RunController<S, E, C, I>
where
    S: SessionStore,
    E: RunExecutor,
    C: Clock,
    I: IdSource,
{
    pub fn new(
        store: Arc<S>,
        executor: Arc<E>,
        clock: Arc<C>,
        ids: Arc<I>,
        retention: RunRetention,
    ) -> Self {
        Self {
            store,
            executor,
            clock,
            ids,
            retention,
            live: Mutex::new(LiveState::new()),
        }
    }

    /// The retention this controller was built with.
    ///
    /// An inbound adapter reads it to size its own memory of answered
    /// requests: remembering more than the run retains would offer a replay
    /// the controller can no longer honour.
    pub const fn retention(&self) -> RunRetention {
        self.retention
    }

    pub async fn start(
        &self,
        request: StartRequest,
        bindings: RunExecutionBindings,
    ) -> Result<RunCommandOutcome, RunError> {
        let session_id = request.session_id;
        match self.read_folded(session_id).await {
            Ok(folded) => return self.replay_start(&request, folded),
            Err(RunError::Store(StoreError::NotFound)) => {}
            Err(error) => return Err(error),
        }

        let run_id = RunId::new(session_id, self.ids.next_uuid());
        let prepared = self
            .executor
            .prepare(&RunPreparation {
                run_id,
                invocation: RunInvocationId::from_uuid(request.request_id.uuid()),
                root: request.root.clone(),
                selections: request.selections.clone(),
                continuation: false,
                origin: request.origin.clone(),
            })
            .await?;
        let (model, prepared) = prepared.into_parts();
        let lease = TurnLease(self.ids.next_uuid());
        let transaction = RunTransaction {
            revision: RunRevision::new(1),
            run_id,
            origin: request.origin.clone(),
            root: request.root.clone(),
            selections: request.selections.clone(),
            command: RunCommand::CommandAdmitted {
                request_id: request.request_id,
                digest: request.digest.clone(),
                expected_revision: None,
                lease,
                executor_id: run_id.nonce,
                prompt: request.prompt.text().to_owned(),
                model: model.clone(),
                start: true,
            },
            status: RunStatus::Running,
            observed_at: self.clock.now(),
            pruned_receipts: Vec::new(),
        };
        match self
            .store
            .compare_and_append_run(&session_id, None, transaction)?
        {
            RunAppend::Committed { revision } => {
                if revision != RunRevision::new(1) {
                    return Err(RunError::Integrity {
                        message: "initial run commit returned a non-initial revision".to_owned(),
                    });
                }
                match self.executor.admit_run_context(&prepared).await {
                    Ok(EvidenceWrite::PublishedWithProjectionWarning { diagnostic }) => {
                        tracing::warn!(%diagnostic, "run evidence manifest projection failed");
                    }
                    Ok(EvidenceWrite::Published | EvidenceWrite::AlreadyRecorded) => {}
                    Err(error) => return Err(self.refuse_evidence(run_id, lease, error).await),
                }
                let mode = ExecutionMode::Submit {
                    run_id,
                    root: request.root.clone(),
                    origin: request.origin.clone(),
                    model,
                    prompt: request.prompt,
                    prepared: Box::new(prepared),
                };
                Ok(RunCommandOutcome::Admitted(self.admission(
                    RunSnapshot {
                        run_id,
                        origin: request.origin,
                        root: request.root,
                        selections: request.selections,
                        revision,
                        status: RunStatus::Running,
                        settlement: None,
                    },
                    lease,
                    bindings,
                    mode,
                )))
            }
            RunAppend::Conflict { .. } => {
                let folded = self.read_folded(session_id).await?;
                self.replay_start(&request, folded)
            }
            RunAppend::CommittedDurabilityUncertain { diagnostic } => {
                Err(RunError::PublishedDurabilityUncertain { run_id, diagnostic })
            }
        }
    }

    pub async fn continue_run(
        &self,
        request: ContinueRequest,
        bindings: RunExecutionBindings,
    ) -> Result<RunCommandOutcome, RunError> {
        let folded = self.read_exact(request.run_id).await?;
        if let Some(receipt) = self.retained_receipt(&folded, request.request_id) {
            return self.replay_receipt(&request.digest, receipt, &folded);
        }
        if folded.snapshot.revision != request.expected_revision {
            return Err(RunError::StaleRevision {
                expected: request.expected_revision,
                actual: folded.snapshot.revision,
            });
        }
        if folded.snapshot.blocks_new_turn() || folded.active.is_some() {
            return Err(RunError::LeaseOccupied {
                run_id: request.run_id,
            });
        }

        let model = self.model_of(&folded);
        let preparation_selections = folded.snapshot.selections.with_model(model.clone());
        let prepared = self
            .executor
            .prepare(&RunPreparation {
                run_id: request.run_id,
                invocation: RunInvocationId::from_uuid(request.request_id.uuid()),
                root: folded.snapshot.root.clone(),
                selections: preparation_selections,
                continuation: true,
                origin: folded.snapshot.origin.clone(),
            })
            .await?;
        let (prepared_model, prepared) = prepared.into_parts();
        if prepared_model != model {
            return Err(RunError::Integrity {
                message: format!(
                    "the continuation prepared {prepared_model} but the run is \
                     pinned to {model}; a resumed run may not change model"
                ),
            });
        }
        let lease = TurnLease(self.ids.next_uuid());
        let revision = folded.snapshot.revision.next();
        let transaction = RunTransaction {
            revision,
            run_id: request.run_id,
            origin: folded.snapshot.origin.clone(),
            root: folded.snapshot.root.clone(),
            selections: folded.snapshot.selections.clone(),
            command: RunCommand::CommandAdmitted {
                request_id: request.request_id,
                digest: request.digest.clone(),
                expected_revision: Some(request.expected_revision),
                lease,
                executor_id: request.run_id.nonce,
                prompt: request.prompt.text().to_owned(),
                model: model.clone(),
                start: false,
            },
            status: RunStatus::Running,
            observed_at: self.clock.now(),
            pruned_receipts: Vec::new(),
        };
        match self.store.compare_and_append_run(
            &request.run_id.session_id,
            Some(folded.snapshot.revision),
            transaction,
        )? {
            RunAppend::Committed {
                revision: committed,
            } => {
                match self.executor.admit_run_context(&prepared).await {
                    Ok(EvidenceWrite::PublishedWithProjectionWarning { diagnostic }) => {
                        tracing::warn!(%diagnostic, "run evidence manifest projection failed");
                    }
                    Ok(EvidenceWrite::Published | EvidenceWrite::AlreadyRecorded) => {}
                    Err(error) => {
                        return Err(self.refuse_evidence(request.run_id, lease, error).await);
                    }
                }
                let mode = ExecutionMode::Submit {
                    run_id: request.run_id,
                    root: folded.snapshot.root.clone(),
                    origin: folded.snapshot.origin.clone(),
                    model,
                    prompt: request.prompt,
                    prepared: Box::new(prepared),
                };
                Ok(RunCommandOutcome::Admitted(self.admission(
                    RunSnapshot {
                        run_id: request.run_id,
                        origin: folded.snapshot.origin,
                        root: folded.snapshot.root,
                        selections: folded.snapshot.selections,
                        revision: committed,
                        status: RunStatus::Running,
                        settlement: None,
                    },
                    lease,
                    bindings,
                    mode,
                )))
            }
            RunAppend::Conflict { .. } => {
                let winner = self.read_exact(request.run_id).await?;
                if let Some(receipt) = self.retained_receipt(&winner, request.request_id) {
                    self.replay_receipt(&request.digest, receipt, &winner)
                } else {
                    Err(RunError::StaleRevision {
                        expected: request.expected_revision,
                        actual: winner.snapshot.revision,
                    })
                }
            }
            RunAppend::CommittedDurabilityUncertain { diagnostic } => {
                Err(RunError::PublishedDurabilityUncertain {
                    run_id: request.run_id,
                    diagnostic,
                })
            }
        }
    }

    pub async fn execute(&self, execution: RunExecution) -> Result<RunSnapshot, RunError> {
        let RunExecution {
            run_id,
            lease,
            bindings,
            mode,
            subscription: _subscription,
            liveness: _liveness,
        } = execution;
        let effect = match mode {
            ExecutionMode::Submit {
                run_id: submitted_run_id,
                root,
                origin,
                model,
                prompt,
                prepared,
            } => {
                if submitted_run_id != run_id {
                    let error = RunError::Integrity {
                        message: "submitted execution carries another run identity".to_owned(),
                    };
                    return Err(self
                        .settle_execution_failure(
                            run_id,
                            lease,
                            error,
                            ExecutionFailure::Interrupted,
                        )
                        .await);
                }
                let prepared = match prepared.downcast::<E::Prepared>() {
                    Ok(prepared) => prepared,
                    Err(_) => {
                        let error = RunError::Integrity {
                            message: "prepared execution capability has the wrong adapter type"
                                .to_owned(),
                        };
                        return Err(self
                            .settle_execution_failure(
                                run_id,
                                lease,
                                error,
                                ExecutionFailure::Interrupted,
                            )
                            .await);
                    }
                };
                let cached = self
                    .live
                    .lock()
                    .expect("run live state")
                    .handles
                    .get(&run_id)
                    .cloned();
                let handle = match cached {
                    Some(handle) => handle,
                    None => {
                        let attachment = RunAttachment {
                            run_id,
                            root,
                            model,
                            origin,
                        };
                        let handle = match self.executor.attach(&attachment).await {
                            Ok(handle) => handle,
                            Err(error) => {
                                return Err(self
                                    .settle_execution_failure(
                                        run_id,
                                        lease,
                                        error.into(),
                                        ExecutionFailure::Uncertain,
                                    )
                                    .await);
                            }
                        };
                        if let Err(error) = self.bind_handle(run_id, &handle) {
                            return Err(self
                                .settle_execution_failure(
                                    run_id,
                                    lease,
                                    error,
                                    ExecutionFailure::Interrupted,
                                )
                                .await);
                        }
                        handle
                    }
                };
                if let Err(error) = self.interrupt_if_cancelling(run_id, lease, &handle).await {
                    let failure = if matches!(&error, RunError::Executor(_)) {
                        ExecutionFailure::Uncertain
                    } else {
                        ExecutionFailure::Interrupted
                    };
                    return Err(self
                        .settle_execution_failure(run_id, lease, error, failure)
                        .await);
                }
                self.executor
                    .submit(&handle, &prompt, *prepared, bindings)
                    .await
            }
            ExecutionMode::RecoveredObserve { handle } => {
                if let Err(error) = self.bind_handle(run_id, &handle) {
                    return Err(self
                        .settle_execution_failure(
                            run_id,
                            lease,
                            error,
                            ExecutionFailure::Interrupted,
                        )
                        .await);
                }
                if let Err(error) = self.interrupt_if_cancelling(run_id, lease, &handle).await {
                    let failure = if matches!(&error, RunError::Executor(_)) {
                        ExecutionFailure::Uncertain
                    } else {
                        ExecutionFailure::Interrupted
                    };
                    return Err(self
                        .settle_execution_failure(run_id, lease, error, failure)
                        .await);
                }
                self.executor.observe(&handle, bindings).await
            }
        };
        let settlement = match effect {
            Ok(settlement) => settlement,
            Err(error) => {
                return Err(self
                    .settle_execution_failure(
                        run_id,
                        lease,
                        error.into(),
                        ExecutionFailure::Uncertain,
                    )
                    .await);
            }
        };
        self.settle(run_id, lease, settlement).await
    }

    pub async fn cancel_admitted(&self, key: &CancelKey) -> Result<RunSnapshot, RunError> {
        self.cancel_inner(&key.request, Some(key.lease)).await
    }

    pub async fn cancel(&self, request: CancelRequest) -> Result<RunSnapshot, RunError> {
        self.cancel_inner(&request, None).await
    }

    /// Ask the run serving this session to stop at its next boundary.
    ///
    /// Answers with the snapshot as it stands, which is how a caller learns
    /// which half of the ADR's contract it is in: a run that is no longer
    /// `Running` has nothing left to hold back, and one that is has been
    /// asked and has not got there yet.
    ///
    /// It appends nothing. The durable record of a pause is the session
    /// log's `SessionPauseRequested`/`SessionPaused` pair, written by the
    /// turn that stops — the one writer holding the lease. A second record
    /// on the run stream would be a second account of the same fact, free to
    /// disagree with the first.
    pub async fn pause(
        &self,
        session: SessionId,
        request: RequestId,
    ) -> Result<RunSnapshot, RunError> {
        let folded = self.read_folded(session).await?;
        if !matches!(
            folded.snapshot.status,
            RunStatus::Running | RunStatus::Cancelling
        ) {
            return Ok(folded.snapshot);
        }
        let handle = {
            self.live
                .lock()
                .expect("run live state")
                .handles
                .get(&folded.snapshot.run_id)
                .cloned()
        };
        // No handle is not an error here, where it is for a cancellation: a
        // cancel that cannot reach its run leaves an effect running, and a
        // pause that cannot reach one leaves a run that will finish the work
        // it was already given. The caller still gets the status and can see
        // for itself that no boundary was reached.
        if let Some(handle) = handle {
            self.executor
                .pause(&handle, request)
                .await
                .map_err(RunError::Executor)?;
        }
        Ok(folded.snapshot)
    }

    pub async fn status(&self, run_id: RunId) -> Result<RunSnapshot, RunError> {
        Ok(self.read_exact(run_id).await?.snapshot)
    }

    /// Resolve the one run stream targeted by a deterministic start request.
    pub async fn status_by_request(&self, request_id: RequestId) -> Result<RunSnapshot, RunError> {
        self.status_by_session(RequestId::session_id(request_id))
            .await
    }

    /// Resolve the one run stream owned by a durable session.
    pub async fn status_by_session(&self, session_id: SessionId) -> Result<RunSnapshot, RunError> {
        Ok(self.read_folded(session_id).await?.snapshot)
    }

    pub async fn recover(
        &self,
        run_id: RunId,
        bindings: RunExecutionBindings,
    ) -> Result<RunRecoveryOutcome, RunError> {
        let folded = self.read_exact(run_id).await?;
        if !matches!(
            folded.snapshot.status,
            RunStatus::Running | RunStatus::Cancelling
        ) {
            let lease = folded.last_lease()?;
            return Ok(RunRecoveryOutcome::Observed(
                self.observation(&folded, lease),
            ));
        }
        let lease = folded.active_lease()?;
        let live_execution = {
            let mut live = self.live.lock().expect("run live state");
            let key = (run_id, lease);
            let liveness = live.executions.get(&key).and_then(Weak::upgrade);
            if liveness.is_none() {
                live.executions.remove(&key);
            }
            liveness
        };
        if let Some(_liveness) = live_execution {
            return Ok(RunRecoveryOutcome::Observed(
                self.observation(&folded, lease),
            ));
        }
        match self.executor.recover(&run_id).await? {
            RecoveryEvidence::Running(handle) => {
                self.bind_handle(run_id, &handle)?;
                Ok(RunRecoveryOutcome::Reattached(self.reattachment(
                    folded.snapshot,
                    lease,
                    handle,
                    bindings,
                )))
            }
            RecoveryEvidence::Terminal => {
                self.settle_and_observe(run_id, lease, RunSettlement::Completed)
                    .await
            }
            RecoveryEvidence::CancelledBeforeStart => {
                self.settle_and_observe(run_id, lease, RunSettlement::CancelledBeforeStart)
                    .await
            }
            // Both settle uncertain, and they are not the same thing: one
            // executor could not tell us, the other told us it cannot. Split
            // so the receipt records which, since a replay of this turn has
            // nothing else left to say.
            RecoveryEvidence::Ambiguous => {
                self.settle_and_observe(
                    run_id,
                    lease,
                    RunSettlement::Uncertain {
                        diagnostic: Some(
                            "the executor could not say whether this turn ran".to_owned(),
                        ),
                    },
                )
                .await
            }
            RecoveryEvidence::NotRecoverable => {
                self.settle_and_observe(
                    run_id,
                    lease,
                    RunSettlement::Uncertain {
                        diagnostic: Some(
                            "the executor cannot recover this turn after a restart".to_owned(),
                        ),
                    },
                )
                .await
            }
        }
    }

    fn admission(
        &self,
        snapshot: RunSnapshot,
        lease: TurnLease,
        bindings: RunExecutionBindings,
        mode: ExecutionMode,
    ) -> RunAdmission {
        let state = Arc::new(Mutex::new(SubscriptionState {
            snapshot: snapshot.clone(),
            complete: false,
            wakers: Vec::new(),
        }));
        let liveness = Arc::new(ExecutionLiveness);
        {
            let mut live = self.live.lock().expect("run live state");
            live.subscriptions
                .insert((snapshot.run_id, lease), Arc::clone(&state));
            live.executions
                .insert((snapshot.run_id, lease), Arc::downgrade(&liveness));
        }
        let cancel_key = self.cancel_key(&snapshot, lease);
        RunAdmission {
            snapshot: snapshot.clone(),
            cancel_key,
            subscription: RunSubscription {
                state: Arc::clone(&state),
            },
            execution: Box::new(RunExecution {
                run_id: snapshot.run_id,
                lease,
                bindings,
                mode,
                subscription: state,
                liveness,
            }),
        }
    }

    fn reattachment(
        &self,
        snapshot: RunSnapshot,
        lease: TurnLease,
        handle: RunHandle,
        bindings: RunExecutionBindings,
    ) -> RunReattachment {
        let state = Arc::new(Mutex::new(SubscriptionState {
            snapshot: snapshot.clone(),
            complete: false,
            wakers: Vec::new(),
        }));
        let liveness = Arc::new(ExecutionLiveness);
        {
            let mut live = self.live.lock().expect("run live state");
            live.subscriptions
                .insert((snapshot.run_id, lease), Arc::clone(&state));
            live.executions
                .insert((snapshot.run_id, lease), Arc::downgrade(&liveness));
        }
        RunReattachment {
            cancel_key: self.cancel_key(&snapshot, lease),
            snapshot: snapshot.clone(),
            subscription: RunSubscription {
                state: Arc::clone(&state),
            },
            execution: Box::new(RunExecution {
                run_id: snapshot.run_id,
                lease,
                bindings,
                mode: ExecutionMode::RecoveredObserve { handle },
                subscription: state,
                liveness,
            }),
        }
    }

    fn cancel_key(&self, snapshot: &RunSnapshot, lease: TurnLease) -> CancelKey {
        CancelKey {
            request: CancelRequest {
                run_id: snapshot.run_id,
                request_id: RequestId(self.ids.next_uuid()),
                digest: RequestDigest("cancel".to_owned()),
                expected_revision: snapshot.revision,
            },
            lease,
        }
    }

    fn bind_handle(&self, run_id: RunId, handle: &RunHandle) -> Result<(), RunError> {
        if handle.run_id != run_id {
            return Err(RunError::Integrity {
                message: "executor returned a handle for another run".to_owned(),
            });
        }
        self.live
            .lock()
            .expect("run live state")
            .handles
            .insert(run_id, handle.clone());
        Ok(())
    }

    async fn interrupt_if_cancelling(
        &self,
        run_id: RunId,
        lease: TurnLease,
        handle: &RunHandle,
    ) -> Result<(), RunError> {
        if self.read_exact(run_id).await?.snapshot.status != RunStatus::Cancelling {
            return Ok(());
        }
        let claimed = self
            .live
            .lock()
            .expect("run live state")
            .interrupted
            .insert((run_id, lease));
        if !claimed {
            return Ok(());
        }
        if let Err(error) = self.executor.interrupt(handle).await {
            self.live
                .lock()
                .expect("run live state")
                .interrupted
                .remove(&(run_id, lease));
            return Err(error.into());
        }
        Ok(())
    }

    async fn settle_execution_failure(
        &self,
        run_id: RunId,
        lease: TurnLease,
        source: RunError,
        failure: ExecutionFailure,
    ) -> RunError {
        let settlement = failure.settlement(&source);
        match self.settle(run_id, lease, settlement).await {
            Ok(_) => source,
            Err(settlement_error) => RunError::ExecutionSettlement {
                source: Box::new(source),
                settlement_diagnostic: settlement_error.to_string(),
            },
        }
    }

    async fn settle_and_observe(
        &self,
        run_id: RunId,
        lease: TurnLease,
        settlement: RunSettlement,
    ) -> Result<RunRecoveryOutcome, RunError> {
        self.settle(run_id, lease, settlement).await?;
        let folded = self.read_exact(run_id).await?;
        Ok(RunRecoveryOutcome::Observed(
            self.observation(&folded, lease),
        ))
    }

    fn observation(&self, folded: &FoldedRun, lease: TurnLease) -> RunObservation {
        self.observation_of(folded, folded.snapshot.clone(), lease)
    }

    /// The observation a retained receipt deserves: the run as it was when
    /// *that* receipt settled.
    ///
    /// A replay is an answer to a request that already happened, so answering
    /// it from the current snapshot reports whatever turn ran most recently.
    /// That is latent today only because the ACP adapter remembers one command
    /// and so never replays anything older; widening that window without this
    /// would make a stale answer reachable.
    fn replayed_observation(
        &self,
        folded: &FoldedRun,
        receipt: &AdmissionReceipt,
    ) -> RunObservation {
        let mut snapshot = folded.snapshot.clone();
        if let Some(settled) = receipt.settled {
            snapshot.revision = settled.revision;
            snapshot.status = settled.status;
            snapshot.settlement = receipt.settlement.clone();
        }
        self.observation_of(folded, snapshot, receipt.lease)
    }

    fn observation_of(
        &self,
        folded: &FoldedRun,
        snapshot: RunSnapshot,
        lease: TurnLease,
    ) -> RunObservation {
        let state = self
            .live
            .lock()
            .expect("run live state")
            .subscriptions
            .get(&(snapshot.run_id, lease))
            .cloned()
            .unwrap_or_else(|| {
                Arc::new(Mutex::new(SubscriptionState {
                    snapshot: snapshot.clone(),
                    complete: folded.active.is_none_or(|active| active.lease != lease),
                    wakers: Vec::new(),
                }))
            });
        RunObservation {
            snapshot,
            subscription: RunSubscription { state },
        }
    }

    fn replay_start(
        &self,
        request: &StartRequest,
        folded: FoldedRun,
    ) -> Result<RunCommandOutcome, RunError> {
        let Some(receipt) = folded.admissions.iter().find(|receipt| receipt.start) else {
            return Err(RunError::IdentityCollision {
                session_id: folded.snapshot.session_id(),
            });
        };
        if !Self::is_retained(receipt) {
            return Err(RunError::IdentityCollision {
                session_id: folded.snapshot.session_id(),
            });
        }
        if receipt.request_id != request.request_id {
            return Err(RunError::IdentityCollision {
                session_id: folded.snapshot.session_id(),
            });
        }
        if receipt.digest != request.digest {
            return Err(RunError::RequestIdMismatch {
                request_id: request.request_id,
            });
        }
        if folded.snapshot.root != request.root {
            return Err(RunError::IdentityCollision {
                session_id: folded.snapshot.session_id(),
            });
        }
        if folded.snapshot.origin != request.origin {
            return Err(RunError::Integrity {
                message: "replayed run request changes its durable origin".to_owned(),
            });
        }
        // `CancelledBeforeStart` is named here because it used to *be*
        // `Interrupted` and matched this check. Splitting the variant must not
        // quietly make a turn retryable that was not retryable yesterday: all
        // three ended before effect authority.
        if matches!(
            receipt.settlement,
            Some(
                RunSettlement::Interrupted { .. }
                    | RunSettlement::CancelledBeforeStart
                    | RunSettlement::Uncertain { .. }
            )
        ) {
            return Err(RunError::Integrity {
                message: "the retained request ended before effect authority and cannot be retried"
                    .to_owned(),
            });
        }
        Ok(RunCommandOutcome::Replayed(
            self.replayed_observation(&folded, receipt),
        ))
    }

    fn replay_receipt(
        &self,
        digest: &RequestDigest,
        receipt: &AdmissionReceipt,
        folded: &FoldedRun,
    ) -> Result<RunCommandOutcome, RunError> {
        if &receipt.digest != digest {
            return Err(RunError::RequestIdMismatch {
                request_id: receipt.request_id,
            });
        }
        Ok(RunCommandOutcome::Replayed(
            self.replayed_observation(folded, receipt),
        ))
    }

    async fn cancel_inner(
        &self,
        request: &CancelRequest,
        required_lease: Option<TurnLease>,
    ) -> Result<RunSnapshot, RunError> {
        let folded = self.read_exact(request.run_id).await?;
        if let Some((digest, lease)) = folded.cancel_receipt(request.request_id) {
            if digest != &request.digest {
                return Err(RunError::RequestIdMismatch {
                    request_id: request.request_id,
                });
            }
            if required_lease.is_some_and(|required| required != lease) {
                return Err(RunError::LeaseOccupied {
                    run_id: request.run_id,
                });
            }
            return Ok(folded.snapshot);
        }
        if folded.snapshot.revision != request.expected_revision {
            return Err(RunError::StaleRevision {
                expected: request.expected_revision,
                actual: folded.snapshot.revision,
            });
        }
        let active = folded.active.ok_or(RunError::LeaseOccupied {
            run_id: request.run_id,
        })?;
        if folded
            .cancels
            .values()
            .any(|(_, lease)| *lease == active.lease)
        {
            return Err(RunError::LeaseOccupied {
                run_id: request.run_id,
            });
        }
        if required_lease.is_some_and(|required| required != active.lease) {
            return Err(RunError::LeaseOccupied {
                run_id: request.run_id,
            });
        }
        let transaction = RunTransaction {
            revision: folded.snapshot.revision.next(),
            run_id: request.run_id,
            origin: folded.snapshot.origin.clone(),
            root: folded.snapshot.root.clone(),
            selections: folded.snapshot.selections.clone(),
            command: RunCommand::CancelRequested {
                request_id: request.request_id,
                digest: request.digest.clone(),
                lease: active.lease,
            },
            status: RunStatus::Cancelling,
            observed_at: self.clock.now(),
            pruned_receipts: Vec::new(),
        };
        match self.store.compare_and_append_run(
            &request.run_id.session_id,
            Some(folded.snapshot.revision),
            transaction,
        )? {
            RunAppend::Committed { revision } => {
                let handle = {
                    self.live
                        .lock()
                        .expect("run live state")
                        .handles
                        .get(&request.run_id)
                        .cloned()
                };
                if let Some(handle) = handle {
                    self.interrupt_if_cancelling(request.run_id, active.lease, &handle)
                        .await?;
                }
                Ok(RunSnapshot {
                    run_id: request.run_id,
                    origin: folded.snapshot.origin,
                    root: folded.snapshot.root,
                    selections: folded.snapshot.selections,
                    revision,
                    status: RunStatus::Cancelling,
                    settlement: None,
                })
            }
            RunAppend::Conflict { .. } => {
                let winner = self.read_exact(request.run_id).await?;
                if winner
                    .cancel_receipt(request.request_id)
                    .is_some_and(|(digest, lease)| {
                        digest == &request.digest
                            && required_lease.is_none_or(|required| required == lease)
                    })
                {
                    return Ok(winner.snapshot);
                }
                Err(RunError::StaleRevision {
                    expected: request.expected_revision,
                    actual: winner.snapshot.revision,
                })
            }
            RunAppend::CommittedDurabilityUncertain { diagnostic } => {
                Err(RunError::PublishedDurabilityUncertain {
                    run_id: request.run_id,
                    diagnostic,
                })
            }
        }
    }

    async fn settle(
        &self,
        run_id: RunId,
        lease: TurnLease,
        settlement: RunSettlement,
    ) -> Result<RunSnapshot, RunError> {
        loop {
            let folded = self.read_exact(run_id).await?;
            if folded.active.map(|active| active.lease) != Some(lease) {
                if let Some(recorded) = folded
                    .admissions
                    .iter()
                    .find(|receipt| receipt.lease == lease)
                    .and_then(|receipt| receipt.settlement.as_ref())
                {
                    if recorded == &settlement {
                        let subscription = {
                            let mut live = self.live.lock().expect("run live state");
                            live.executions.remove(&(run_id, lease));
                            live.subscriptions.get(&(run_id, lease)).cloned()
                        };
                        if let Some(subscription) = subscription {
                            Self::publish_subscription(
                                &subscription,
                                folded.snapshot.clone(),
                                true,
                            );
                        }
                        return Ok(folded.snapshot);
                    }
                    return Err(RunError::Integrity {
                        message: "one lease has conflicting settlements".to_owned(),
                    });
                }
                return Err(RunError::Integrity {
                    message: "settlement does not own the active lease".to_owned(),
                });
            }
            let status = settlement.status();
            let transaction = RunTransaction {
                revision: folded.snapshot.revision.next(),
                run_id,
                origin: folded.snapshot.origin.clone(),
                root: folded.snapshot.root.clone(),
                selections: folded.snapshot.selections.clone(),
                command: RunCommand::CommandSettled {
                    lease,
                    settlement: settlement.clone(),
                },
                status,
                observed_at: self.clock.now(),
                pruned_receipts: self.pruning_after_settlement(&folded, lease),
            };
            match self.store.compare_and_append_run(
                &run_id.session_id,
                Some(folded.snapshot.revision),
                transaction,
            )? {
                RunAppend::Committed { revision } => {
                    let snapshot = RunSnapshot {
                        run_id,
                        origin: folded.snapshot.origin,
                        root: folded.snapshot.root,
                        selections: folded.snapshot.selections,
                        revision,
                        status,
                        settlement: Some(settlement.clone()),
                    };
                    let subscription = {
                        let mut live = self.live.lock().expect("run live state");
                        live.executions.remove(&(run_id, lease));
                        live.subscriptions.get(&(run_id, lease)).cloned()
                    };
                    if let Some(subscription) = subscription {
                        Self::publish_subscription(&subscription, snapshot.clone(), true);
                    }
                    return Ok(snapshot);
                }
                RunAppend::Conflict { .. } => continue,
                RunAppend::CommittedDurabilityUncertain { diagnostic } => {
                    // The turn is over for anyone waiting on it, and this
                    // process cannot say how it ended. Releasing the observer
                    // with that terminal uncertainty is the only honest
                    // answer; leaving it parked orphans the request instead.
                    let snapshot = RunSnapshot {
                        run_id,
                        origin: folded.snapshot.origin,
                        root: folded.snapshot.root,
                        selections: folded.snapshot.selections,
                        revision: folded.snapshot.revision,
                        status: RunStatus::Uncertain,
                        settlement: Some(RunSettlement::Uncertain {
                            diagnostic: Some(diagnostic.clone()),
                        }),
                    };
                    let subscription = {
                        let mut live = self.live.lock().expect("run live state");
                        live.executions.remove(&(run_id, lease));
                        live.subscriptions.get(&(run_id, lease)).cloned()
                    };
                    if let Some(subscription) = subscription {
                        Self::publish_subscription(&subscription, snapshot, true);
                    }
                    return Err(RunError::PublishedDurabilityUncertain { run_id, diagnostic });
                }
            }
        }
    }

    async fn refuse_evidence(
        &self,
        run_id: RunId,
        lease: TurnLease,
        source: EvidenceError,
    ) -> RunError {
        let diagnostic = Some(source.to_string());
        let settlement = if matches!(source, EvidenceError::DurabilityUncertain { .. }) {
            RunSettlement::Uncertain { diagnostic }
        } else {
            RunSettlement::Interrupted { diagnostic }
        };
        let settlement_diagnostic = self
            .settle(run_id, lease, settlement)
            .await
            .err()
            .map(|error| error.to_string());
        RunError::Evidence {
            source: Box::new(source),
            settlement_diagnostic,
        }
    }

    async fn read_exact(&self, run_id: RunId) -> Result<FoldedRun, RunError> {
        let folded = self.read_folded(run_id.session_id).await?;
        if folded.snapshot.run_id != run_id {
            return Err(RunError::NotFound { run_id });
        }
        Ok(folded)
    }

    async fn read_folded(&self, session: SessionId) -> Result<FoldedRun, RunError> {
        let mut stream = self.store.read(&session)?;
        let mut transactions = Vec::new();
        while let Some(entry) = stream.next().await {
            if let SessionEntry::RunTransaction(transaction) = entry? {
                transactions.push(transaction);
            }
        }
        FoldedRun::from_transactions(transactions)
    }

    fn retained_receipt<'a>(
        &self,
        folded: &'a FoldedRun,
        request_id: RequestId,
    ) -> Option<&'a AdmissionReceipt> {
        folded
            .admissions
            .iter()
            .find(|receipt| receipt.request_id == request_id && Self::is_retained(receipt))
    }

    fn is_retained(receipt: &AdmissionReceipt) -> bool {
        !receipt.pruned
    }

    fn pruning_after_settlement(&self, folded: &FoldedRun, lease: TurnLease) -> Vec<TurnLease> {
        let retained: Vec<TurnLease> = folded
            .admissions
            .iter()
            .filter(|receipt| {
                !receipt.pruned && (receipt.settlement.is_some() || receipt.lease == lease)
            })
            .map(|receipt| receipt.lease)
            .collect();
        let excess = retained
            .len()
            .saturating_sub(self.retention.completed_receipts.get());
        retained.into_iter().take(excess).collect()
    }

    fn model_of(&self, folded: &FoldedRun) -> ModelRef {
        folded
            .admissions
            .first()
            .expect("a folded run has an admission")
            .model
            .clone()
    }

    fn publish_subscription(
        state: &Arc<Mutex<SubscriptionState>>,
        snapshot: RunSnapshot,
        complete: bool,
    ) {
        let wakers = {
            let mut state = state.lock().expect("run subscription");
            state.snapshot = snapshot;
            state.complete = complete;
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    fn release_live_attachment(&self, run_id: RunId, lease: TurnLease) {
        let mut live = self.live.lock().expect("run live state");
        live.executions.remove(&(run_id, lease));
        live.subscriptions.remove(&(run_id, lease));
        live.handles.remove(&run_id);
    }
}

impl<S, E, C, I> SessionRunLifecycle for RunController<S, E, C, I>
where
    S: SessionStore,
    E: RunExecutor,
    C: Clock,
    I: IdSource,
{
    fn activity(&self, session: SessionId) -> ActivityFuture<'_> {
        Box::pin(async move {
            let folded = match self.read_folded(session).await {
                Ok(folded) => folded,
                Err(RunError::Store(StoreError::NotFound)) => {
                    return Ok(SessionActivity::Inactive);
                }
                Err(error) => return Err(lifecycle_run_error(error)),
            };
            Ok(match folded.snapshot.status {
                RunStatus::Running | RunStatus::Cancelling => SessionActivity::Active,
                RunStatus::Uncertain => SessionActivity::Uncertain,
                // Paused is inactive: the turn stopped and nothing is
                // running. Whether the *session* may be given more work is a
                // different question, answered by `SessionState::pause`.
                RunStatus::Settled
                | RunStatus::Paused
                | RunStatus::Cancelled
                | RunStatus::Interrupted => SessionActivity::Inactive,
            })
        })
    }

    fn hold(&self, session: SessionId, request: RequestId) -> HoldSignalFuture<'_> {
        Box::pin(async move {
            let snapshot = match self.pause(session, request).await {
                Ok(snapshot) => snapshot,
                // No run stream, read exactly as `activity` reads it: this
                // session has never run, so there is no turn a hold could
                // reach. Not `UnknownSession` — the caller established the
                // session exists before asking, and a session created and
                // paused before its first prompt is an ordinary thing to do.
                Err(RunError::Store(StoreError::NotFound)) => return Ok(false),
                Err(error) => return Err(lifecycle_run_error(error)),
            };
            Ok(snapshot.status == RunStatus::Running)
        })
    }

    fn cancel_and_settle(&self, session: SessionId) -> CloseFuture<'_> {
        Box::pin(async move {
            let folded = match self.read_folded(session).await {
                Ok(folded) => folded,
                Err(RunError::Store(StoreError::NotFound)) => {
                    return Ok(ClosedSession {
                        id: session,
                        cancelled: false,
                    });
                }
                Err(error) => return Err(lifecycle_run_error(error)),
            };
            match folded.snapshot.status {
                RunStatus::Uncertain => {
                    if let Ok(lease) = folded.last_lease() {
                        self.release_live_attachment(folded.snapshot.run_id, lease);
                    }
                    return Err(LifecycleError::EffectUncertain);
                }
                // Paused belongs with the terminal statuses and not with
                // `Cancelling`: a paused run has already stopped at a durable
                // boundary, so there is no in-flight effect to cancel and
                // nothing for this close to wait on.
                RunStatus::Settled
                | RunStatus::Paused
                | RunStatus::Cancelled
                | RunStatus::Interrupted => {
                    if let Ok(lease) = folded.last_lease() {
                        self.release_live_attachment(folded.snapshot.run_id, lease);
                    }
                    return Ok(ClosedSession {
                        id: session,
                        cancelled: false,
                    });
                }
                RunStatus::Running | RunStatus::Cancelling => {}
            }

            let lease = folded.active_lease().map_err(lifecycle_run_error)?;
            let subscription = self
                .live
                .lock()
                .expect("run live state")
                .subscriptions
                .get(&(folded.snapshot.run_id, lease))
                .cloned()
                .map(|state| RunSubscription { state })
                .ok_or(LifecycleError::EffectUncertain)?;
            if folded.snapshot.status == RunStatus::Running {
                let key = self.cancel_key(&folded.snapshot, lease);
                self.cancel_admitted(&key)
                    .await
                    .map_err(lifecycle_run_error)?;
            }
            let settled = subscription.await;
            if settled.status == RunStatus::Uncertain {
                return Err(LifecycleError::EffectUncertain);
            }
            self.release_live_attachment(folded.snapshot.run_id, lease);
            Ok(ClosedSession {
                id: session,
                cancelled: matches!(
                    settled.status,
                    RunStatus::Cancelled | RunStatus::Interrupted
                ),
            })
        })
    }
}

fn lifecycle_run_error(error: RunError) -> LifecycleError {
    match error {
        RunError::PublishedDurabilityUncertain { .. } => LifecycleError::EffectUncertain,
        RunError::Store(error) => LifecycleError::Store(error),
        other => LifecycleError::Run(other.to_string()),
    }
}

impl FoldedRun {
    fn from_transactions(transactions: Vec<RunTransaction>) -> Result<Self, RunError> {
        let first = transactions
            .first()
            .ok_or(RunError::Store(StoreError::NotFound))?;
        if !matches!(&first.command, RunCommand::CommandAdmitted { .. }) {
            return Err(RunError::Integrity {
                message: "the first run transaction is not a command admission".to_owned(),
            });
        }
        let run_id = first.run_id;
        let root = first.root.clone();
        let origin = first.origin.clone();
        let selections = first.selections.clone();
        let mut admissions: Vec<AdmissionReceipt> = Vec::new();
        let mut active: Option<ActiveLease> = None;
        let mut expected = RunRevision::new(1);
        let mut previous_revision = None;
        let mut admitted_model: Option<ModelRef> = None;
        let mut admitted_leases = BTreeSet::new();
        let mut snapshot = RunSnapshot {
            run_id,
            origin: first.origin.clone(),
            root: root.clone(),
            selections: selections.clone(),
            revision: expected,
            status: RunStatus::Running,
            settlement: None,
        };
        let mut cancels = BTreeMap::new();
        let mut pruned = BTreeSet::new();
        for transaction in &transactions {
            if transaction.run_id != run_id
                || transaction.origin != origin
                || transaction.root != root
                || transaction.selections != selections
                || transaction.revision != expected
            {
                return Err(RunError::Integrity {
                    message: "run transaction stream changes identity, origin, root, selections, or revision order"
                        .to_owned(),
                });
            }
            match &transaction.command {
                RunCommand::CommandAdmitted {
                    expected_revision,
                    lease,
                    executor_id,
                    model,
                    start,
                    ..
                } => {
                    if *start != previous_revision.is_none()
                        || *expected_revision != previous_revision
                    {
                        return Err(RunError::Integrity {
                            message: "run admission start and expected revision do not match its stream position"
                                .to_owned(),
                        });
                    }
                    if transaction.status != RunStatus::Running {
                        return Err(RunError::Integrity {
                            message: "command admission does not carry running status".to_owned(),
                        });
                    }
                    if *executor_id != run_id.nonce {
                        return Err(RunError::Integrity {
                            message: "run admission executor identity does not match the run nonce"
                                .to_owned(),
                        });
                    }
                    if admitted_model
                        .as_ref()
                        .is_some_and(|admitted| admitted != model)
                    {
                        return Err(RunError::Integrity {
                            message: "run admission changes the immutable model".to_owned(),
                        });
                    }
                    if active.is_some() || admitted_leases.contains(lease) {
                        return Err(RunError::Integrity {
                            message: "run admission does not own one new inactive lease".to_owned(),
                        });
                    }
                }
                RunCommand::CancelRequested {
                    request_id, lease, ..
                } => {
                    if transaction.status != RunStatus::Cancelling {
                        return Err(RunError::Integrity {
                            message: "cancel request does not carry cancelling status".to_owned(),
                        });
                    }
                    if active.map(|value| value.lease) != Some(*lease)
                        || cancels.values().any(|(_, existing)| existing == lease)
                        || cancels.contains_key(request_id)
                    {
                        return Err(RunError::Integrity {
                            message: "cancel receipt does not name exactly one active lease"
                                .to_owned(),
                        });
                    }
                }
                RunCommand::CommandSettled { lease, settlement } => {
                    if transaction.status != settlement.status() {
                        return Err(RunError::Integrity {
                            message: "command settlement status disagrees with its settlement"
                                .to_owned(),
                        });
                    }
                    if active.map(|value| value.lease) != Some(*lease)
                        || admissions
                            .iter()
                            .find(|receipt| receipt.lease == *lease)
                            .is_none_or(|receipt| receipt.settlement.is_some())
                    {
                        return Err(RunError::Integrity {
                            message: "duplicate or foreign settlement".to_owned(),
                        });
                    }
                }
            }

            let settling_lease = match &transaction.command {
                RunCommand::CommandSettled { lease, .. } => Some(*lease),
                _ => None,
            };
            if settling_lease.is_none() && !transaction.pruned_receipts.is_empty() {
                return Err(RunError::Integrity {
                    message: "receipt pruning is not carried by a command settlement".to_owned(),
                });
            }
            let mut newly_pruned = BTreeSet::new();
            for lease in &transaction.pruned_receipts {
                if pruned.contains(lease) || !newly_pruned.insert(*lease) {
                    return Err(RunError::Integrity {
                        message: "one receipt was pruned more than once".to_owned(),
                    });
                }
                let receipt = admissions
                    .iter()
                    .find(|receipt| receipt.lease == *lease)
                    .ok_or_else(|| RunError::Integrity {
                        message: "receipt pruning names no admission".to_owned(),
                    })?;
                if receipt.settlement.is_none() && settling_lease != Some(*lease) {
                    return Err(RunError::Integrity {
                        message: "unsettled receipt was pruned".to_owned(),
                    });
                }
            }

            match &transaction.command {
                RunCommand::CommandAdmitted {
                    request_id,
                    digest,
                    lease,
                    start,
                    model,
                    ..
                } => {
                    admissions.push(AdmissionReceipt {
                        request_id: *request_id,
                        digest: digest.clone(),
                        lease: *lease,
                        settlement: None,
                        settled: None,
                        start: *start,
                        model: model.clone(),
                        pruned: false,
                    });
                    admitted_model.get_or_insert_with(|| model.clone());
                    admitted_leases.insert(*lease);
                    active = Some(ActiveLease { lease: *lease });
                    snapshot.settlement = None;
                }
                RunCommand::CancelRequested {
                    request_id,
                    digest,
                    lease,
                } => {
                    cancels.insert(*request_id, (digest.clone(), *lease));
                }
                RunCommand::CommandSettled { lease, settlement } => {
                    let receipt = admissions
                        .iter_mut()
                        .find(|receipt| receipt.lease == *lease)
                        .expect("validated settlement has an admission");
                    receipt.settlement = Some(settlement.clone());
                    // Read from the transaction rather than from `snapshot`,
                    // which the two lines below this match arm have not
                    // advanced yet. This is the receipt's own terminal state
                    // and the only chance to record it.
                    receipt.settled = Some(SettledOutcome {
                        revision: transaction.revision,
                        status: transaction.status,
                    });
                    active = None;
                    snapshot.settlement = Some(settlement.clone());
                }
            }
            snapshot.revision = transaction.revision;
            snapshot.status = transaction.status;
            for lease in &transaction.pruned_receipts {
                pruned.insert(*lease);
                let receipt = admissions
                    .iter_mut()
                    .find(|receipt| receipt.lease == *lease)
                    .expect("validated pruning has an admission");
                receipt.pruned = true;
                cancels.retain(|_, (_, cancel_lease)| *cancel_lease != *lease);
            }
            previous_revision = Some(transaction.revision);
            expected = expected.next();
        }
        Ok(Self {
            snapshot,
            admissions,
            cancels,
            active,
        })
    }

    fn active_lease(&self) -> Result<TurnLease, RunError> {
        self.active
            .map(|active| active.lease)
            .ok_or_else(|| RunError::Integrity {
                message: "run has no active lease to recover".to_owned(),
            })
    }

    fn last_lease(&self) -> Result<TurnLease, RunError> {
        self.admissions
            .last()
            .map(|receipt| receipt.lease)
            .ok_or_else(|| RunError::Integrity {
                message: "run has no admitted lease to observe".to_owned(),
            })
    }

    fn cancel_receipt(&self, request_id: RequestId) -> Option<(&RequestDigest, TurnLease)> {
        self.cancels
            .get(&request_id)
            .map(|(digest, lease)| (digest, *lease))
    }
}

// Minimal SHA-1 used solely for the fixed UUIDv5 derivation above.
fn sha1(input: &[u8]) -> [u8; 20] {
    let bit_len = (input.len() as u64) * 8;
    let mut bytes = input.to_vec();
    bytes.push(0x80);
    while bytes.len() % 64 != 56 {
        bytes.push(0);
    }
    bytes.extend_from_slice(&bit_len.to_be_bytes());
    let mut h = [
        0x67452301_u32,
        0xefcdab89,
        0x98badcfe,
        0x10325476,
        0xc3d2e1f0,
    ];
    for chunk in bytes.as_chunks::<64>().0 {
        let mut w = [0_u32; 80];
        for (index, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            w[index] = (w[index - 3] ^ w[index - 8] ^ w[index - 14] ^ w[index - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (index, word) in w.into_iter().enumerate() {
            let (f, k) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5a827999),
                20..=39 => (b ^ c ^ d, 0x6ed9eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1bbcdc),
                _ => (b ^ c ^ d, 0xca62c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut output = [0_u8; 20];
    for (chunk, word) in output.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    output
}
