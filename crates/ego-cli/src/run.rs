//! One headless run, start to exit code.
//!
//! Everything this module decides, it decides once and hands to something
//! else: the exit code is `RunTermination::exit_code`, the projection is
//! `project_public`, the totals are `SessionState`'s. What is genuinely the
//! binary's own is the *order* — config, then instructions, then a model, then
//! a store, then turns — and the two things only a process can do: notice a
//! signal, and notice a deadline.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::time::{Duration, Instant};

use ego_core::budget::SharedBudgetGate;
use ego_core::ceiling::{Budget, Ceilings};
use ego_core::compact::{
    CompactionCommand, CompactionError, CompactionFuture, CompactionPreparation, CompactionReceipt,
    CompactionTransitionError, CompactionTransitionRequest, CompactionTransitionService,
    PreparedCompaction,
};
use ego_core::config::rule_management::RuleManagement;
use ego_core::config::{
    ConfigLayer, Effective, Limits, McpConfigProvenance, McpServerConfig, McpServerName, Mode,
    ModelRef, PermCfg, Posture, RuleMutation, RuleSnapshot, Ruleset, Selection, Source, Sourced,
    SystemPrompt, Trust, UserConfigError, UserConfigMutation, UserConfigPublication,
    UserConfigStore, UserConfigTarget, resolve_selected, system_prompt,
};
use ego_core::cost::CurrencyCode;
use ego_core::event::domain::CancelCause;
use ego_core::event::domain::EventKind;
use ego_core::event::hook::{HookLifecycle, HookOccurrenceKey, HookOccurrenceOwner, HookSubject};
use ego_core::event::id::{EventId, ItemId, Producer, SessionId, SubagentInvocationId, TurnId};
use ego_core::evidence::{
    ConfigProvenance, EgoVersion, EvidenceAdmissionService, EvidenceDigest, EvidenceError,
    EvidenceRecord, EvidenceWrite, ExecutionAdmissionDescriptor, McpContextDescriptor,
    McpTransportKind, RunContextEvidence, RunInvocationId,
};
use ego_core::generation_model_source::{
    GenerationModelError, GenerationModelErrorClass, GenerationModelRequest, GenerationProfile,
    GenerationRole,
};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::generator::port::RequestExpressibilityError;
use ego_core::generator::types::{GenRequest, Image, ImageRef, ThinkingLevel, ToolDescriptor};
use ego_core::hook::{
    HookCatalogue, HookCausality, HookEvent, HookInvocationBinding, HookLifecycleDispatcher,
    HookOccurrence, HookRunner, HookStartedEvidence, SessionHookRuntime, SessionHookState,
};
use ego_core::mcp::McpRuntimeBinding;
use ego_core::permit::judge::{JudgeConfig, JudgeInput};
use ego_core::permit::residual::ResidualWriteDesk;
use ego_core::permit::{
    CanonPath, Change, Confinement, Escalation, ExecCaps, ExecObservation, HumanApprover,
    MissingExecCaps, Op, OpName, PermissionDecision, PermissionRequest, PolicyDigest, PolicyEngine,
    PreparedCall, RunConfinement,
};
use ego_core::public::{
    InstructionDescriptor, PUBLIC_EVENT_VERSION, PublicEvent, QuestionReport, project_public,
};
use ego_core::question::{
    AskQuestionTool, Elicitor, QuestionAnswer, QuestionError, QuestionOutcome, QuestionTarget,
};
use ego_core::run::{
    CancelReason, DeniedBy, IdSource, RecoveryEvidence, RequestId, RunAttachment, RunController,
    RunExecutionBindings, RunExecutor, RunExecutorError, RunHandle, RunId, RunPreparation,
    RunPrepared, RunPrompt, RunRetention, RunRoot, RunSelections, RunSettlement, RunSnapshot,
    RunTermination, Signal,
};
use ego_core::session_lifecycle::{LeasedSharedJournal, SessionMetadata};
use ego_core::sink::{EventSink, NullSink};
use ego_core::skill::{
    AdmittedSkillOperation, SkillError, SkillInvocation, SkillInvocationOutcome,
    SkillInvocationPort,
};
use ego_core::state::{ItemKind, QuestionContinuation, SessionState};
use ego_core::store::blob::{BlobId, BlobRef, BlobStore};
use ego_core::store::run_artifact::{
    ArtifactPublication, PublishedRunArtifact, RunArtifactId, RunArtifactStore,
};
use ego_core::store::session_store::{SessionEntry, SessionStore};
use ego_core::subagent::{
    ChildHost, ChildJournalError, ChildRequest, ChildRequestOrigin, ChildResultEvidence,
    ChildResultReaderError, ChildRunService, ChildServiceError, ChildServicePorts, ChildWait,
    RecoveryCut,
};
use ego_core::turn::{
    AdmittedToolSource, AgentIdentity, CancelToken, GenerationPolicySource, GenerationRuntime,
    InteractiveControl, Journal, JudgeContext, PauseToken, QueueStatus, RegisteredTool, Session,
    SharedJournal, ToolEffectPorts, ToollessGenerationOwner, Tools, TurnAbandoned, TurnConfig,
    TurnLoop, TurnOutcome, interactive_control,
};
use ego_core::usage::{UsagePeriod, UsageReportService};
use ego_mcp::McpRegistryController;
use ego_tools::clock::SystemClock;
use ego_tools::config::OsUserConfigStore;
use ego_tools::exec::{ProcessError, RunEffectPorts};
use ego_tools::fs::OsFs;
use ego_tools::instructions::OsInstructionIo;
use ego_tools::store::{JsonlStore, OsIo, SessionEvidenceStore};
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

mod executor;
mod session_commands;
mod turns;
mod windows_unconfined;

use executor::{
    ChildAuthorityCeiling, DeferredCompactionPreparation, compaction_termination,
    complete_run_context, public_result_artifact, run_executor_error, settlement,
};
pub use executor::{CliCompactionCommand, CliRunExecutor};
pub(crate) use executor::{protocol_args, protocol_effective};
pub use session_commands::{compact, export, fork, ls, rm};
use session_commands::{exported, finalize_ci_bundle, finalize_ci_error};
use turns::{
    Driver, InteractiveStatus, RunHost, SeatApprover, Turns, last_assistant_message, run_trust,
};
pub use turns::{InteractiveSeat, PasteboardKind, Reads, stop_signal};

use crate::budget;
use crate::cli::{Args, ModelsArgs, Output, ResumeAnswer, SessionChoice, SessionShape, UsageError};
use crate::hook::{CliHookInvocationBinding, RunToollessGenerationService};
use crate::model_picker::{ModelPicker, ModelSwitchRequest};
use crate::models;
use crate::output::{
    Diagnostics, InteractiveOutput, JsonlSink, Report, Stderr, Stdout, TextOutput, TextSink, note,
};
use crate::provider_http::{ProviderHttpAdapter, run_effect_ports};
use crate::render::caps::{Capabilities, Environment, OscProfile, Terminal};
use crate::render::osc::{Osc, Progress, State};
use crate::repl::policy_controls::{
    CliGenerationPolicySource, FrozenPolicyResolver, FrozenRuleResolution,
    GenerationPolicySelection, GenerationPolicySnapshot, PermissionInput, PermissionOutcome,
    PolicyApplicationOwner, PolicyControlError, PolicyControlOutcome, PolicyControlRequest,
    PreflightPort, RememberPreview,
};
use crate::session::{self, ActiveSession, Opened};
use crate::{attachment, config, hooks, model, prompt, repl, schema};

mod interactive_driver;
use interactive_driver::*;
pub use interactive_driver::{
    Ended, RunModelSwitchError, RunModelSwitchOwner, apply_resume_answers_once,
    interactive_with_mcp_registry, run_with_ci_preflight, run_with_mcp_registry,
};
pub(crate) mod composition;
use composition::*;
pub(crate) use composition::{FrozenRuleResolver, rooted_path, workspace};
mod port_adapters;
pub use port_adapters::cancelled_termination;
use port_adapters::*;
mod headless_driver;
use headless_driver::*;
mod composition_root;
pub(crate) mod profile_roots;
pub(crate) mod read_directory;
use composition_root::*;
pub use composition_root::{CliPrepared, DirectPreflight, preflight_ci};
mod execution;
use execution::{ActivatedRun, MaterializedRun, ScopedInvocation, execute};
mod driver_runtime;
use driver_runtime::{DriverExecution, drive_execution};

/// The result of a run, rendered for `--output json` (`CLI.md` §4.1).
pub fn report(ended: &Ended) -> Report {
    let mut report = Report::of(
        ended.session,
        &ended.state,
        &ended.termination,
        ended.result.clone(),
    );
    report.mcp_errors = ended.mcp_errors.clone();
    report
}

/// The one line text mode owes at the end when something went wrong.
///
/// A run that exits non-zero with nothing on stderr has told a person only a
/// number. `--output json` says it in the object instead, and saying it twice
/// would put diagnostics on stdout.
///
/// `broken` is what [`Stdout`] recorded if a write failed. A run stopped by a
/// closed pipe ends as `Signalled(Term)`, which on its own reads as "somebody
/// sent SIGTERM"; naming the real cause is the difference between a report and
/// a guess.
pub fn diagnose(termination: &RunTermination, broken: Option<String>) {
    let message = match termination {
        RunTermination::Done => return,
        RunTermination::TaskFailed => "the answer never satisfied the schema".to_string(),
        RunTermination::UsageError { message }
        | RunTermination::Provider { message }
        | RunTermination::ProviderUncertain { message }
        | RunTermination::ContextOverflow { message } => message.clone(),
        RunTermination::Failed { message } => message.clone(),
        RunTermination::QuestionRequired { question } => format!(
            "stopped: human input required — answer with `ego resume --answer {}=JSON`",
            question.as_ref().id
        ),
        // Two sentences, because one of them was about to be false. "Nobody
        // could grant it" describes a question the evaluation order exhausted;
        // said of a refusal somebody made, it denies that anybody decided,
        // which is the one thing that did happen. Both still lead with the
        // class — a person who reads only this line has to see that exit 3 is
        // about permission — and both name the action, whose own `Display`
        // carries the rule, the mode or the boundary that produced it.
        RunTermination::PermissionRequired {
            action,
            by: DeniedBy::Decided(_),
        } => format!("stopped: permission refused — {action}"),
        RunTermination::PermissionRequired {
            action,
            by: DeniedBy::Nobody,
        } => format!("stopped: permission unresolved and nobody could be asked — {action}"),
        RunTermination::BudgetExceeded => "stopped: the budget was reached".to_string(),
        // Reachable from `ego run` even though nothing here can pause: the
        // hold is durable, so a session paused over ACP stays paused when the
        // next turn is asked for from another surface. Saying so is the point
        // — the alternative is a run that looks like it did nothing.
        RunTermination::Paused => {
            "stopped: the session is paused; resume it from the host that paused it".to_string()
        }
        RunTermination::MaxTurns => "stopped: --max-turns was reached".to_string(),
        RunTermination::TimedOut => "stopped: --timeout expired".to_string(),
        RunTermination::Signalled(Signal::Int) => "interrupted".to_string(),
        RunTermination::Signalled(Signal::Term) => match broken {
            Some(why) => format!("stopped: {why}"),
            None => "terminated".to_string(),
        },
    };
    note(&message);
}

#[cfg(test)]
#[path = "run/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "run/phase_boundary_tests.rs"]
mod phase_boundary_tests;

#[cfg(test)]
#[path = "../tests/support/mod.rs"]
pub(crate) mod test_support;

/// Inspect only IDs admitted in the current session's durable journal.
fn background_tasks(
    arguments: &str,
    state: &SessionState,
    process: &dyn ego_core::permit::ProcessExecutor,
) -> String {
    use ego_core::event::id::DetachedExecId;
    let words: Vec<_> = arguments.split_whitespace().collect();
    let (selected, cancel) = match words.as_slice() {
        [] => (None, false),
        [id] => (Some(*id), false),
        ["cancel", id] => (Some(*id), true),
        _ => return "usage: /tasks [ID | cancel ID]".into(),
    };
    let selected = match selected.map(str::parse::<DetachedExecId>).transpose() {
        Ok(id) => id,
        Err(_) => return "invalid background exec ID".into(),
    };
    if selected.is_some_and(|id| !state.detached_execs.contains_key(&id)) {
        return "unknown background exec ID in this session".into();
    }
    let mut rows = Vec::new();
    for (id, settled) in &state.detached_execs {
        if selected.is_some_and(|selected| selected != *id) {
            continue;
        }
        if cancel
            && settled.is_none()
            && let Err(error) = process.cancel_background(*id)
        {
            return error.to_string();
        }
        let snapshot = match settled
            .clone()
            .map(Ok)
            .unwrap_or_else(|| process.background_status(*id))
        {
            Ok(snapshot) => snapshot,
            Err(ego_core::permit::ProcessError::UnknownBackground { .. }) => {
                ego_core::permit::DetachedExecSnapshot {
                    id: *id,
                    outcome: Some(ego_core::permit::DetachedExecOutcome::Interrupted),
                    stdout_tail: Vec::new(),
                    stderr_tail: Vec::new(),
                    stdout_omitted: 0,
                    stderr_omitted: 0,
                }
            }
            Err(error) => return error.to_string(),
        };
        rows.push(format!("exec {}", snapshot.model_text()));
        if cancel && snapshot.outcome.is_none() {
            rows.push("cancellation requested".into());
        }
    }
    if rows.is_empty() {
        "background tasks: none".into()
    } else {
        rows.join("\n")
    }
}

/// Both idle and busy inspection use the same typed task surface.
async fn task_listing(
    arguments: &str,
    state: &SessionState,
    process: &dyn ego_core::permit::ProcessExecutor,
    owner: &CliSkillInvocationOwner<'_>,
) -> String {
    let agents = owner.inspect_tasks(arguments, state).await;
    let execs = background_tasks(arguments, state, process);
    match agents {
        Some(agents) if arguments.trim().is_empty() && !state.detached_execs.is_empty() => {
            format!("{execs}\n{agents}")
        }
        Some(agents) => agents,
        None => execs,
    }
}
