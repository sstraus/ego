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
#[derive(Debug, Clone, PartialEq, Eq)]
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
