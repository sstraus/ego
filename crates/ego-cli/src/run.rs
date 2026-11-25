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
    ConfigLayer, Effective, McpConfigProvenance, McpServerConfig, McpServerName, Mode, ModelRef,
    PermCfg, Posture, RuleMutation, RuleSnapshot, Ruleset, Selection, Source, Sourced,
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
    InteractiveControl, Journal, JudgeContext, PauseToken, RegisteredTool, Session, SharedJournal,
    ToolEffectPorts, ToollessGenerationOwner, Tools, TurnAbandoned, TurnConfig, TurnLoop,
    TurnOutcome, interactive_control,
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

/// TUICommander manages this session: it sets `TUIC_SESSION` for its children
/// and sends its own reprompt when a turn ends without the report (story 939-8c87).
pub(crate) fn managed_by_tuic() -> bool {
    std::env::var_os("TUIC_SESSION").is_some_and(|session| !session.is_empty())
}

mod background_indicator;
mod executor;
mod mcp_task_wake;
mod provider_switch;
use provider_switch::{ProviderDestination, ProviderSwitch};
mod rewind_command;
mod root_revocation;
mod session_commands;
mod startup_failure;
mod turns;
mod windows_unconfined;

pub(crate) use composition::captured_session_policy;
use executor::{
    ChildAuthorityCeiling, DeferredCompactionPreparation, compaction_termination,
    complete_run_context, public_result_artifact, run_executor_error, settlement,
};
pub use executor::{CliCompactionCommand, CliRunExecutor};
pub(crate) use executor::{protocol_args, protocol_effective};
pub use rewind_command::rewind;
pub use session_commands::{compact, export, fork, ls, publish_resume_fork, rm};
use session_commands::{exported, finalize_ci_bundle, finalize_ci_error};
use turns::{
    Driver, InteractiveStatus, RootTransitionContext, RootTransitionSlot, RunHost, SeatRootGrants,
    SeatSandboxGrants, Turns, last_assistant_message, run_trust, seat_human, take_root_transition,
};
pub use turns::{InteractiveSeat, PasteboardKind, Reads, stop_signal};

use crate::budget;
use crate::cli::{Args, Output, ResumeAnswer, SessionChoice, SessionShape, UsageError};
use crate::hook::{CliHookInvocationBinding, RunToollessGenerationService};
use crate::model_picker::{ModelPicker, ModelSwitchRequest};
use crate::models;
use crate::output::{
    Diagnostics, InteractiveOutput, JsonlSink, Report, Stderr, Stdout, TextOutput, TextSink,
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
pub(crate) use interactive_driver::native_activity;
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
mod headless_jsonl;
use headless_driver::*;
mod composition_root;
pub(crate) mod profile_roots;
pub(crate) mod read_directory;
use composition_root::*;
pub use composition_root::{CliPrepared, DirectPreflight, preflight_ci};
mod execution;
pub(crate) mod startup_layout;
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
pub fn diagnose(termination: &RunTermination, out: &Stdout) {
    let message = match termination {
        RunTermination::Done => return,
        RunTermination::TaskFailed => "the task did not complete successfully".to_string(),
        RunTermination::UsageError { message }
        | RunTermination::Guard { message }
        | RunTermination::Provider { message }
        | RunTermination::ProviderUncertain { message }
        | RunTermination::ContextOverflow { message } => message.clone(),
        RunTermination::Failed { message } => message.clone(),
        RunTermination::QuestionRequired { question } => format!(
            "stopped: your answer is needed — respond with 'ego resume --answer {}=JSON'",
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
        RunTermination::Signalled(Signal::Term) => match out.broken() {
            Some(why) => format!("stopped: {why}"),
            None => "terminated".to_string(),
        },
    };
    let error = matches!(
        termination,
        RunTermination::UsageError { .. }
            | RunTermination::Guard { .. }
            | RunTermination::Provider { .. }
            | RunTermination::ProviderUncertain { .. }
            | RunTermination::ContextOverflow { .. }
            | RunTermination::Failed { .. }
            | RunTermination::TaskFailed
    );
    out.final_note(&message, error);
}

#[cfg(test)]
#[path = "run/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "run/phase_boundary_tests.rs"]
mod phase_boundary_tests;

#[cfg(test)]
#[path = "run/mcp_batch_e2e_tests.rs"]
mod mcp_batch_e2e_tests;

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
        [] | ["details"] => (None, false),
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
    let mut table = vec![vec!["Exec".into(), "State".into()]];
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
        if selected.is_none() && arguments != "details" {
            use ego_core::permit::DetachedExecOutcome;
            let status = match snapshot.outcome.as_ref() {
                None => "running".into(),
                Some(DetachedExecOutcome::Exited { code, .. }) => format!("exited {code}"),
                Some(DetachedExecOutcome::Signalled { .. }) => "signalled".into(),
                Some(DetachedExecOutcome::Cancelled) => "cancelled".into(),
                Some(DetachedExecOutcome::TimedOut { .. }) => "timed out".into(),
                Some(DetachedExecOutcome::Interrupted) => "interrupted".into(),
                Some(DetachedExecOutcome::Failed { .. }) => "failed".into(),
            };
            table.push(vec![
                format!("…{}", id.to_string().rsplit('-').next().unwrap_or_default()),
                status,
            ]);
        } else {
            rows.push(format!("exec {}", snapshot.model_text()));
        }
        if cancel && snapshot.outcome.is_none() {
            rows.push("cancellation requested".into());
        }
    }
    if table.len() > 1 {
        rows.extend(crate::render::table::plain_table(table));
        rows.push("/tasks details — full identities and output".into());
    }
    if rows.is_empty() {
        "background tasks: none".into()
    } else {
        rows.join("\n")
    }
}

/// Foreground identities use the existing task item, not durable aliases.
fn foreground_task_listing(arguments: &str, state: &SessionState) -> Option<String> {
    let words: Vec<_> = arguments.split_whitespace().collect();
    let selected = match words.as_slice() {
        [] | ["details"] => None,
        [id] => Some(*id),
        _ => return None,
    };
    let mut items: Vec<_> = state
        .items
        .iter()
        .filter(|(id, item)| {
            item.tool_name.as_deref() == Some("task")
                && selected.is_none_or(|selected| selected == id.to_string())
        })
        .collect();
    items.sort_by_key(|(_, item)| item.seq);
    let mut details = Vec::new();
    let mut rows: Vec<_> = items
        .into_iter()
        .filter_map(|(id, item)| {
            let args: Value = serde_json::from_str(&item.args).ok()?;
            if args.get("background").and_then(Value::as_bool) == Some(true) {
                return None;
            }
            let name = ego_core::permit::terminal_safe(
                args.get("name").and_then(Value::as_str).unwrap_or("task"),
            );
            let status = match item.ok {
                None => "running",
                Some(true) => "completed",
                Some(false) => "failed",
            };
            let profile = state
                .task_profiles
                .get(id)
                .map(|profile| {
                    format!(
                        "{} · {}",
                        ego_core::permit::terminal_safe(&profile.model.0),
                        profile.effort.as_str()
                    )
                })
                .unwrap_or_else(|| {
                    if item.ok.is_none() {
                        "resolving"
                    } else {
                        "profile unavailable"
                    }
                    .to_owned()
                });
            let fork = if args.get("fork").and_then(Value::as_bool) == Some(true) {
                " · fork"
            } else {
                ""
            };
            if selected.is_some() || arguments == "details" {
                details.push(format!("task {id} {name}: {status} · {profile}{fork}"));
                None
            } else {
                Some(vec![name, status.into(), format!("{profile}{fork}")])
            }
        })
        .collect();
    if !rows.is_empty() {
        rows.insert(0, vec!["Task".into(), "State".into(), "Profile".into()]);
        details.extend(crate::render::table::plain_table(rows));
        details.push("/tasks details — full task identities".into());
    }
    (!details.is_empty()).then(|| details.join("\n"))
}

/// Both idle and busy inspection use the same typed task surface.
async fn task_listing(
    arguments: &str,
    state: &SessionState,
    process: &dyn ego_core::permit::ProcessExecutor,
    owner: &CliSkillInvocationOwner<'_>,
) -> String {
    let agents = owner.inspect_tasks(arguments, state).await;
    let foreground = foreground_task_listing(arguments, state);
    let agents = match (agents, foreground) {
        (Some(agents), Some(foreground)) => Some(format!("{agents}\n{foreground}")),
        (Some(rows), None) | (None, Some(rows)) => Some(rows),
        (None, None) => None,
    };
    let execs = background_tasks(arguments, state, process);
    match agents {
        Some(agents)
            if matches!(arguments.trim(), "" | "details") && !state.detached_execs.is_empty() =>
        {
            format!("{execs}\n{agents}")
        }
        Some(agents) => agents,
        None => execs,
    }
}
