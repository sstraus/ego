//! Why an *invocation* ended, as opposed to why a turn did.
//!
//! See `docs/decisions/2026-08-21-two-levels-of-ending.md` and `CLI.md` §4.3.
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
use crate::permit::Authority;
use crate::public::QuestionReport;

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures::StreamExt;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::ModelRef;
use crate::event::id::SessionId;
use crate::event::time::{Clock, Timestamp};
use crate::generator::capabilities::ModelCapabilities;
use crate::question::Elicitor;
use crate::sink::EventSink;
use crate::store::session_store::{SessionEntry, SessionStore, StoreError};

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
/// `docs/decisions/2026-08-21-an-exit-code-that-names-who-refused.md` §2.
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
    QuestionRequired { question: QuestionReport },
    /// The provider or the network failed.
    Provider { message: String },
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
            RunTermination::Provider { .. } => 4,
            RunTermination::BudgetExceeded => 5,
            RunTermination::MaxTurns => 5,
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
            RunTermination::TaskFailed
            | RunTermination::UsageError { .. }
            | RunTermination::Provider { .. }
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
                Self::QuestionRequired { question } => Some(question.clone()),
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
    /// `docs/decisions/2026-08-21-a-request-the-model-will-not-take.md`.
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
    pub fn after_turn(reason: ExitReason, denied: Option<Denial>) -> Option<Self> {
        match reason {
            ExitReason::Completed => None,
            ExitReason::Budget => Some(RunTermination::BudgetExceeded),
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
    fn new(session_id: SessionId, nonce: Uuid) -> Self {
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

/// Public durable lifecycle state. `Running` means admitted, not inferred
/// provider activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Cancelling,
    Settled,
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
    Cancelled { reason: CancelReason },
    PermissionRequired { action: String, by: DeniedBy },
    Interrupted,
    Uncertain,
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
            Self::Completed | Self::PermissionRequired { .. } => RunStatus::Settled,
            Self::Cancelled { .. } => RunStatus::Cancelled,
            Self::Interrupted => RunStatus::Interrupted,
            Self::Uncertain => RunStatus::Uncertain,
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
    root: RunRoot,
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

    pub fn root(&self) -> &RunRoot {
        &self.root
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

/// First prompt and immutable selections for a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    root: RunRoot,
    request_id: RequestId,
    digest: RequestDigest,
    requested_model: Option<ModelRef>,
    prompt: String,
}

impl StartRequest {
    pub fn new(
        root: RunRoot,
        request_id: RequestId,
        digest: RequestDigest,
        requested_model: Option<ModelRef>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            root,
            request_id,
            digest,
            requested_model,
            prompt: prompt.into(),
        }
    }
}

/// A later prompt, guarded by the revision observed by its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinueRequest {
    run_id: RunId,
    request_id: RequestId,
    digest: RequestDigest,
    expected_revision: RunRevision,
    prompt: String,
}

impl ContinueRequest {
    pub fn new(
        run_id: RunId,
        request_id: RequestId,
        digest: RequestDigest,
        expected_revision: RunRevision,
        prompt: impl Into<String>,
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
    pub root: RunRoot,
    pub requested_model: Option<ModelRef>,
    pub continuation: bool,
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

/// Executor start request for the first admitted prompt.
#[derive(Debug, Clone)]
pub struct RunStart {
    pub run_id: RunId,
    pub root: RunRoot,
    pub model: ModelRef,
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
}

impl RunExecutionBindings {
    pub fn new(
        sink_factory: Arc<dyn EventSinkFactory>,
        elicitor: Option<Arc<dyn Elicitor>>,
    ) -> Self {
        Self {
            sink_factory,
            elicitor,
        }
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

    fn start<'a>(
        &'a self,
        request: &'a RunStart,
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
        prompt: &'a str,
        prepared: Self::Prepared,
        bindings: RunExecutionBindings,
    ) -> BoxFuture<'a, Result<RunSettlement, RunExecutorError>>;

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
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
            Self::Integrity { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for RunError {}

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
    root: RunRoot,
    revision: RunRevision,
    status: RunStatus,
    settlement: Option<RunSettlement>,
}

impl RunSnapshot {
    pub const fn run_id(&self) -> RunId {
        self.run_id
    }
    pub const fn session_id(&self) -> SessionId {
        self.run_id.session_id()
    }
    pub fn root(&self) -> RunRoot {
        self.root.clone()
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
    Start {
        root: RunRoot,
        model: ModelRef,
        prompt: String,
        prepared: Box<dyn Any + Send>,
    },
    Continue {
        prompt: String,
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
    start: bool,
    model: ModelRef,
    pruned: bool,
}

#[derive(Clone, Copy)]
struct ActiveLease {
    lease: TurnLease,
}

struct LiveState {
    handles: HashMap<RunId, RunHandle>,
    subscriptions: HashMap<(RunId, TurnLease), Arc<Mutex<SubscriptionState>>>,
    interrupted: BTreeSet<(RunId, TurnLease)>,
}

impl LiveState {
    fn new() -> Self {
        Self {
            handles: HashMap::new(),
            subscriptions: HashMap::new(),
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

    pub async fn start(
        &self,
        request: StartRequest,
        bindings: RunExecutionBindings,
    ) -> Result<RunCommandOutcome, RunError> {
        let session_id = RequestId::session_id(request.request_id);
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
                root: request.root.clone(),
                requested_model: request.requested_model.clone(),
                continuation: false,
            })
            .await?;
        let (model, prepared) = prepared.into_parts();
        let lease = TurnLease(self.ids.next_uuid());
        let transaction = RunTransaction {
            revision: RunRevision::new(1),
            run_id,
            root: request.root.clone(),
            command: RunCommand::CommandAdmitted {
                request_id: request.request_id,
                digest: request.digest.clone(),
                expected_revision: None,
                lease,
                executor_id: run_id.nonce,
                prompt: request.prompt.clone(),
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
                let mode = ExecutionMode::Start {
                    root: request.root.clone(),
                    model,
                    prompt: request.prompt,
                    prepared: Box::new(prepared),
                };
                Ok(RunCommandOutcome::Admitted(self.admission(
                    RunSnapshot {
                        run_id,
                        root: request.root,
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
        let prepared = self
            .executor
            .prepare(&RunPreparation {
                run_id: request.run_id,
                root: folded.snapshot.root.clone(),
                requested_model: Some(model.clone()),
                continuation: true,
            })
            .await?;
        let (prepared_model, prepared) = prepared.into_parts();
        if prepared_model != model {
            return Err(RunError::Integrity {
                message: format!(
                    "continuation preparation model mismatch: expected {model:?}, actual {prepared_model:?}"
                ),
            });
        }
        let lease = TurnLease(self.ids.next_uuid());
        let revision = folded.snapshot.revision.next();
        let transaction = RunTransaction {
            revision,
            run_id: request.run_id,
            root: folded.snapshot.root.clone(),
            command: RunCommand::CommandAdmitted {
                request_id: request.request_id,
                digest: request.digest.clone(),
                expected_revision: Some(request.expected_revision),
                lease,
                executor_id: request.run_id.nonce,
                prompt: request.prompt.clone(),
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
                let mode = ExecutionMode::Continue {
                    prompt: request.prompt,
                    prepared: Box::new(prepared),
                };
                Ok(RunCommandOutcome::Admitted(self.admission(
                    RunSnapshot {
                        run_id: request.run_id,
                        root: folded.snapshot.root,
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
        } = execution;
        let effect = match mode {
            ExecutionMode::Start {
                root,
                model,
                prompt,
                prepared,
            } => {
                let prepared =
                    prepared
                        .downcast::<E::Prepared>()
                        .map_err(|_| RunError::Integrity {
                            message: "prepared execution capability has the wrong adapter type"
                                .to_owned(),
                        })?;
                let handle = self
                    .executor
                    .start(&RunStart {
                        run_id,
                        root,
                        model,
                    })
                    .await?;
                self.bind_handle(run_id, &handle)?;
                self.interrupt_if_cancelling(run_id, lease, &handle).await?;
                self.executor
                    .submit(&handle, &prompt, *prepared, bindings)
                    .await
            }
            ExecutionMode::Continue { prompt, prepared } => {
                let prepared =
                    prepared
                        .downcast::<E::Prepared>()
                        .map_err(|_| RunError::Integrity {
                            message: "prepared execution capability has the wrong adapter type"
                                .to_owned(),
                        })?;
                let handle = self
                    .live
                    .lock()
                    .expect("run live state")
                    .handles
                    .get(&run_id)
                    .cloned()
                    .ok_or_else(|| RunError::Integrity {
                        message: "continuation has no live or recovered executor handle".to_owned(),
                    })?;
                self.interrupt_if_cancelling(run_id, lease, &handle).await?;
                self.executor
                    .submit(&handle, &prompt, *prepared, bindings)
                    .await
            }
            ExecutionMode::RecoveredObserve { handle } => {
                self.bind_handle(run_id, &handle)?;
                self.interrupt_if_cancelling(run_id, lease, &handle).await?;
                self.executor.observe(&handle, bindings).await
            }
        };
        let settlement = match effect {
            Ok(settlement) => settlement,
            Err(error) => {
                self.settle(run_id, lease, RunSettlement::Uncertain).await?;
                return Err(error.into());
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

    pub async fn status(&self, run_id: RunId) -> Result<RunSnapshot, RunError> {
        Ok(self.read_exact(run_id).await?.snapshot)
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
                self.settle_and_observe(run_id, lease, RunSettlement::Interrupted)
                    .await
            }
            RecoveryEvidence::Ambiguous | RecoveryEvidence::NotRecoverable => {
                self.settle_and_observe(run_id, lease, RunSettlement::Uncertain)
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
        self.live
            .lock()
            .expect("run live state")
            .subscriptions
            .insert((snapshot.run_id, lease), Arc::clone(&state));
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
        self.live
            .lock()
            .expect("run live state")
            .subscriptions
            .insert((snapshot.run_id, lease), Arc::clone(&state));
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
        let state = self
            .live
            .lock()
            .expect("run live state")
            .subscriptions
            .get(&(folded.snapshot.run_id, lease))
            .cloned()
            .unwrap_or_else(|| {
                Arc::new(Mutex::new(SubscriptionState {
                    snapshot: folded.snapshot.clone(),
                    complete: folded.active.is_none_or(|active| active.lease != lease),
                    wakers: Vec::new(),
                }))
            });
        RunObservation {
            snapshot: folded.snapshot.clone(),
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
        Ok(RunCommandOutcome::Replayed(
            self.observation(&folded, receipt.lease),
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
            self.observation(folded, receipt.lease),
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
            root: folded.snapshot.root.clone(),
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
                    root: folded.snapshot.root,
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
                        if let Some(subscription) = self
                            .live
                            .lock()
                            .expect("run live state")
                            .subscriptions
                            .get(&(run_id, lease))
                            .cloned()
                        {
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
                root: folded.snapshot.root.clone(),
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
                        root: folded.snapshot.root,
                        revision,
                        status,
                        settlement: Some(settlement.clone()),
                    };
                    if let Some(subscription) = self
                        .live
                        .lock()
                        .expect("run live state")
                        .subscriptions
                        .get(&(run_id, lease))
                        .cloned()
                    {
                        Self::publish_subscription(&subscription, snapshot.clone(), true);
                    }
                    return Ok(snapshot);
                }
                RunAppend::Conflict { .. } => continue,
                RunAppend::CommittedDurabilityUncertain { diagnostic } => {
                    return Err(RunError::PublishedDurabilityUncertain { run_id, diagnostic });
                }
            }
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
        let mut admissions: Vec<AdmissionReceipt> = Vec::new();
        let mut active: Option<ActiveLease> = None;
        let mut expected = RunRevision::new(1);
        let mut previous_revision = None;
        let mut admitted_model: Option<ModelRef> = None;
        let mut admitted_leases = BTreeSet::new();
        let mut snapshot = RunSnapshot {
            run_id,
            root: root.clone(),
            revision: expected,
            status: RunStatus::Running,
            settlement: None,
        };
        let mut cancels = BTreeMap::new();
        let mut pruned = BTreeSet::new();
        for transaction in &transactions {
            if transaction.run_id != run_id
                || transaction.root != root
                || transaction.revision != expected
            {
                return Err(RunError::Integrity {
                    message: "run transaction stream changes identity, root, or revision order"
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
    for chunk in bytes.chunks_exact(64) {
        let mut w = [0_u32; 80];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
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
    for (chunk, word) in output.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    output
}
