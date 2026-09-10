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
    AdmittedToolSource, CancelToken, GenerationPolicySource, GenerationRuntime, InteractiveControl,
    Journal, JudgeContext, PauseToken, QueueStatus, RegisteredTool, Session, SharedJournal,
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
use ego_tools::tools::builtins;
use futures::{FutureExt, StreamExt};
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

mod executor;
mod session_commands;
mod turns;

pub(crate) use executor::protocol_effective;
use executor::{
    ChildAuthorityCeiling, DeferredCompactionPreparation, compaction_termination,
    complete_run_context, protocol_args, public_result_artifact, run_executor_error, settlement,
};
pub use executor::{CliCompactionCommand, CliRunExecutor};
pub use session_commands::{compact, export, fork, ls};
use session_commands::{exported, finalize_ci_bundle, finalize_ci_error};
use turns::{
    Driver, InteractiveStatus, RunHost, SeatApprover, Turns, last_assistant_message, run_trust,
};
pub use turns::{InteractiveSeat, Reads, stop_signal};

use crate::budget;
use crate::cli::{Args, ModelsArgs, Output, ResumeAnswer, SessionChoice, UsageError};
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

/// The currency a session settles in.
///
/// One constant and not a flag, because `--currency` is not in `CLI.md` §3 and
/// a currency nobody can state is worse than a stated one: every quote this
/// build can produce is either zero (a local daemon) or supplied alongside its
/// own currency.
const SETTLEMENT: &str = "USD";

/// The active session's asynchronous authority for one staged model switch.
#[derive(Clone)]
pub struct RunModelSwitchOwner<'s> {
    selection: Arc<model::GenerationSelection>,
    config_store: Arc<dyn UserConfigStore>,
    hooks: SessionHookRuntime<'s>,
    turn: TurnId,
    parent: Option<EventId>,
}

impl<'s> RunModelSwitchOwner<'s> {
    pub fn new(
        selection: Arc<model::GenerationSelection>,
        config_store: Arc<dyn UserConfigStore>,
        hooks: SessionHookRuntime<'s>,
        turn: TurnId,
        parent: Option<EventId>,
    ) -> Self {
        Self {
            selection,
            config_store,
            hooks,
            turn,
            parent,
        }
    }

    /// Gate, publish, and install a picker request at the real run owner.
    pub async fn apply(
        &self,
        request: ModelSwitchRequest,
    ) -> Result<Option<UserConfigPublication>, RunModelSwitchError> {
        let target = request.target();
        let thinking = request.thinking();
        let current = self
            .selection
            .snapshot()
            .map_err(|error| RunModelSwitchError::Selection(error.to_string()))?;
        if current.identity == target
            && current.thinking == thinking
            && current.explicit_thinking == Some(thinking)
        {
            return Ok(None);
        }

        let request_id = RequestId::from_uuid(uuid::Uuid::now_v7());
        let pre = model_switch_occurrence(
            HookLifecycle::PreModelSwitch,
            request_id,
            &target,
            HookCausality::request(request_id, self.turn, self.parent),
        )?;
        let pre = self
            .hooks
            .dispatch(Producer::Main, pre)
            .await
            .map_err(|error| RunModelSwitchError::PreDispatch(error.to_string()))?;
        if pre.is_blocked() {
            return Err(RunModelSwitchError::PreBlocked);
        }

        let publication = if request.is_persistent() {
            let result = self
                .config_store
                .mutate(UserConfigMutation::ModelAndThinking {
                    model: target.0.clone(),
                    thinking,
                })
                .map_err(RunModelSwitchError::Persistence)?;
            Some(result.publication)
        } else {
            None
        };
        if let Err(message) = self.selection.install(target.clone(), thinking) {
            return Err(RunModelSwitchError::Install {
                message,
                publication,
            });
        }

        let post = model_switch_occurrence(
            HookLifecycle::PostModelSwitch,
            request_id,
            &target,
            HookCausality::request(request_id, self.turn, pre.closure_event()),
        )?;
        let post = self
            .hooks
            .dispatch(Producer::Main, post)
            .await
            .map_err(|error| RunModelSwitchError::PostDispatch {
                message: error.to_string(),
                publication: publication.clone(),
            })?;
        if post.is_observer_failure() {
            return Err(RunModelSwitchError::PostObserver { publication });
        }
        Ok(publication)
    }
}

fn model_switch_occurrence(
    lifecycle: HookLifecycle,
    request: RequestId,
    target: &ModelRef,
    causality: HookCausality,
) -> Result<HookOccurrence, RunModelSwitchError> {
    HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Request { request }),
        HookEvent::tool(lifecycle, HookSubject::Session, target.0.clone()),
        causality,
    )
    .map_err(|error| RunModelSwitchError::Occurrence(error.to_string()))
}

#[derive(Debug)]
pub enum RunModelSwitchError {
    Selection(String),
    Occurrence(String),
    PreDispatch(String),
    PreBlocked,
    Persistence(UserConfigError),
    Install {
        message: String,
        publication: Option<UserConfigPublication>,
    },
    PostDispatch {
        message: String,
        publication: Option<UserConfigPublication>,
    },
    PostObserver {
        publication: Option<UserConfigPublication>,
    },
}

impl RunModelSwitchError {
    pub fn publication(&self) -> Option<&UserConfigPublication> {
        match self {
            Self::Install { publication, .. }
            | Self::PostDispatch { publication, .. }
            | Self::PostObserver { publication } => publication.as_ref(),
            Self::Selection(_)
            | Self::Occurrence(_)
            | Self::PreDispatch(_)
            | Self::PreBlocked
            | Self::Persistence(_) => None,
        }
    }
}

impl std::fmt::Display for RunModelSwitchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selection(message) => {
                write!(
                    formatter,
                    "the active model selection could not be read: {message}"
                )
            }
            Self::Occurrence(message) => {
                write!(
                    formatter,
                    "the model-switch lifecycle is invalid: {message}"
                )
            }
            Self::PreDispatch(message) => {
                write!(formatter, "the pre-model-switch gate failed: {message}")
            }
            Self::PreBlocked => formatter.write_str("the model switch was blocked by a hook"),
            Self::Persistence(error) => {
                write!(formatter, "user model defaults were not published: {error}")
            }
            Self::Install { message, .. } => {
                write!(
                    formatter,
                    "the active model selection was not installed: {message}"
                )
            }
            Self::PostDispatch { message, .. } => {
                write!(
                    formatter,
                    "the post-model-switch observer failed: {message}"
                )
            }
            Self::PostObserver { .. } => {
                formatter.write_str("a post-model-switch observer failed after commit")
            }
        }
    }
}

impl std::error::Error for RunModelSwitchError {}

enum ResumePlan {
    Model {
        answer: QuestionAnswer,
        continuation: QuestionContinuation,
        request_event: EventId,
    },
    Foreign {
        question: ItemId,
        continuation_id: String,
        answer: QuestionAnswer,
    },
}

fn plan_resume_answer(
    state: &SessionState,
    answers: &[ResumeAnswer],
) -> Result<ResumePlan, RunTermination> {
    let usage = |message: String| RunTermination::UsageError { message };
    if answers.is_empty() {
        return Err(usage("resume answer batch is empty".to_string()));
    }
    let distinct = answers
        .iter()
        .map(|answer| answer.id)
        .collect::<std::collections::HashSet<_>>();
    if distinct.len() != answers.len() {
        return Err(usage(
            "a question answer was supplied more than once".to_string(),
        ));
    }
    if answers.len() != 1 {
        return Err(usage(
            "exactly one answer is accepted for the pending question".to_string(),
        ));
    }

    let supplied = &answers[0];
    let answer = state
        .answer_pending(supplied.id, &supplied.json)
        .map_err(|error| usage(error.to_string()))?;
    let pending = state
        .pending_question()
        .expect("answer validation found the pending question");
    match pending.target() {
        QuestionTarget::ModelTool { .. } => {
            let continuation = state
                .resume_question(supplied.id, answer.clone())
                .map_err(|error| usage(error.to_string()))?;
            Ok(ResumePlan::Model {
                answer,
                continuation,
                request_event: pending.request_event(),
            })
        }
        QuestionTarget::ForeignContinuation {
            continuation_id, ..
        } => Ok(ResumePlan::Foreign {
            question: pending.item_id(),
            continuation_id: continuation_id.clone(),
            answer,
        }),
    }
}

/// Validate and apply the sole answer batch against one already-opened
/// durable session. Validation is complete before the first journal append.
pub fn apply_resume_answers_once(
    opened: &mut Opened<'_>,
    answers: &[ResumeAnswer],
) -> Result<QuestionContinuation, RunTermination> {
    let state = opened.state();
    match plan_resume_answer(&state, answers)? {
        ResumePlan::Model {
            answer,
            continuation,
            request_event,
        } => apply_model_resume_once(opened, answer, continuation, request_event),
        ResumePlan::Foreign { .. } => Err(RunTermination::UsageError {
            message: QuestionError::ForeignContinuationRequired.to_string(),
        }),
    }
}

fn apply_model_resume_once(
    opened: &mut Opened<'_>,
    answer: QuestionAnswer,
    continuation: QuestionContinuation,
    request_event: EventId,
) -> Result<QuestionContinuation, RunTermination> {
    let state = opened.state();
    let Opened::Durable(open) = opened;
    let clock = SystemClock;
    let sink = NullSink;
    let mut journal = Journal::new(
        open.writer.id(),
        &open.writer,
        &clock,
        &sink,
        Producer::Main,
        state,
    );
    let settlement = journal
        .append(
            continuation.turn,
            Some(continuation.question),
            Some(request_event),
            EventKind::QuestionSettled {
                outcome: QuestionOutcome::Answered(answer),
            },
        )
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    journal
        .append(
            continuation.turn,
            Some(continuation.tool_call),
            Some(settlement),
            EventKind::ToolCallCompleted {
                ok: continuation.ok(),
                result: continuation.tool_result.clone(),
            },
        )
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    open.state = journal.into_state();
    Ok(continuation)
}

/// How long a cancelled turn is given to unwind before the run stops waiting.
///
/// `CancelToken` is checked before each generation and after each stream event,
/// so a provider that has gone silent delays cancellation until its transport
/// gives up. Waiting forever would make `--timeout` a suggestion; not waiting
/// at all would abandon an unsettled attempt, which is what `AttemptStream`
/// exists to prevent. A bounded wait is the only honest third answer.
const GRACE: Duration = Duration::from_secs(5);

/// Complete stdout or stderr beyond this per-stream bound fails the tool and
/// terminates its process tree. Preview memory is bounded separately by
/// [`ego_tools::exec::ExecLimits`].
const EVIDENCE_CAP: u64 = 64 * 1024 * 1024;

/// What the run produced, beyond how it ended.
pub struct Ended {
    pub termination: RunTermination,
    pub session: SessionId,
    pub state: SessionState,
    /// The final answer: the validated document under `--schema`, the last
    /// assistant message otherwise, and the empty string when there was none.
    pub result: Value,
    /// One canonical, sanitized projection of MCP startup failures.
    pub mcp_errors: Vec<String>,
}

impl Ended {
    /// A run that ended before a session existed.
    fn refused(termination: RunTermination) -> Self {
        Ended {
            termination,
            session: SessionId::new(),
            state: SessionState::default(),
            result: Value::String(String::new()),
            mcp_errors: Vec::new(),
        }
    }
}

pub async fn run_with_mcp_registry(
    args: Args,
    out: &Stdout,
    cancel: CancelToken,
    evidence: Arc<EvidenceAdmissionService>,
) -> Ended {
    run_process(args, out, cancel, evidence, None).await
}

/// Run one direct CI invocation from the exact preflight snapshot captured
/// before the invocation-confined MCP authority is opened.
pub async fn run_with_ci_preflight(
    args: Args,
    out: &Stdout,
    cancel: CancelToken,
    evidence: Arc<EvidenceAdmissionService>,
    preflight: DirectPreflight,
) -> Ended {
    run_process(args, out, cancel, evidence, Some(preflight.0)).await
}

async fn run_process(
    mut args: Args,
    out: &Stdout,
    cancel: CancelToken,
    evidence: Arc<EvidenceAdmissionService>,
    preflight: Option<ExecutionPreflight>,
) -> Ended {
    config::capture_profile(&mut args);
    let output = args.output;
    let quiet = args.quiet;
    // The one place the terminal is asked what it is. §4.5's "stdout is
    // protocol-clean" is why the machine-readable modes are not asked at all
    // rather than asked and ignored: there is no environment in which a JSON
    // document acquires an escape sequence.
    let environment = Environment::read();
    let capabilities = match output {
        Output::Text => Capabilities::of(&Terminal::from_process(args.no_color, &environment)),
        Output::Json | Output::Jsonl => Capabilities::plain(),
    };
    let text_sink = TextSink::new(out, quiet, capabilities);
    let jsonl_sink = JsonlSink::new(out);
    let sink: &dyn EventSink = match output {
        Output::Text => &text_sink,
        Output::Jsonl => &jsonl_sink,
        // §4.1 is a single object at completion, so nothing is published as it
        // happens — but the run still needs a sink, and one that writes
        // nothing is the honest one rather than a collector nobody reads.
        Output::Json => &ego_core::sink::NullSink,
    };

    let announce = Announce::new(out, capabilities, quiet);
    let opening_prompt = args.prompt.clone();

    let ended = run_with_registry(
        args,
        |_| {
            announce.opening(opening_prompt.as_deref());
            sink
        },
        cancel,
        evidence,
        preflight,
    )
    .await;

    // Draws the last block and closes the last line. A no-op unless text mode
    // has something outstanding, which is why it can run on the refusal path
    // too: a run that refused before the first token held nothing back.
    if output == Output::Text {
        text_sink.finish();
    }
    announce.closing(&ended.termination);
    ended
}

pub async fn interactive_with_mcp_registry(
    args: Args,
    out: &Stdout,
    cancel: CancelToken,
    evidence: Arc<EvidenceAdmissionService>,
) -> Ended {
    interactive_process(args, out, cancel, evidence).await
}

async fn interactive_process(
    mut args: Args,
    out: &Stdout,
    cancel: CancelToken,
    evidence: Arc<EvidenceAdmissionService>,
) -> Ended {
    config::capture_profile(&mut args);
    if args.profile.as_deref() == Some("ci") {
        return Ended::refused(RunTermination::UsageError {
            message: "profile `ci` is available only through ego run or ego resume".to_owned(),
        });
    }
    // A machine-readable stream has no reader here. `--output json` is a
    // single document at completion and a conversation has no completion, so
    // the flag is refused by name rather than quietly producing a stream
    // nobody can consume — `CLI.md` §4.5.
    if args.output != Output::Text {
        return Ended::refused(RunTermination::UsageError {
            message: format!(
                "`--output {}` has no meaning for an interactive session: it is \
                 one document at completion, and a conversation does not have \
                 one. Use `ego run --output {}` for a headless one-shot",
                args.output, args.output
            ),
        });
    }

    let quiet = args.quiet;
    let environment = Environment::read();
    let capabilities = Capabilities::of(&Terminal::from_process(args.no_color, &environment));
    let (seat, surface) = match repl::Seat::new(cancel.clone()) {
        Ok(built) => built,
        Err(err) => {
            return Ended::refused(RunTermination::Failed {
                message: format!("could not start the terminal owner: {err}"),
            });
        }
    };
    let seat = Arc::new(seat);
    let services = InteractiveServices { evidence };

    if let Some(surface) = surface {
        let output = InteractiveOutput::new(surface);
        let ended = interactive_through(
            args,
            cancel,
            Arc::clone(&seat),
            capabilities,
            quiet,
            &output,
            services,
        )
        .await;
        output.finish();
        seat.shutdown();
        drop(output);
        seat.join();
        ended
    } else {
        interactive_through(
            args,
            cancel,
            Arc::clone(&seat),
            capabilities,
            quiet,
            out,
            services,
        )
        .await
    }
}

struct InteractiveServices {
    evidence: Arc<EvidenceAdmissionService>,
}

async fn interactive_through(
    args: Args,
    cancel: CancelToken,
    seat: Arc<repl::Seat>,
    capabilities: Capabilities,
    quiet: bool,
    output: &dyn TextOutput,
    services: InteractiveServices,
) -> Ended {
    let text_sink = TextSink::new(output, quiet, capabilities);
    let announce = Announce::new(output, capabilities, quiet);
    let interactive_seat: Arc<dyn InteractiveSeat> = seat.clone();
    let elicitor: Arc<dyn Elicitor> = seat.clone();
    let human = interactive_seat.has_human();
    let status = InteractiveStatus {
        seat: Arc::clone(&seat),
        width: capabilities.width,
        human,
        native: human && capabilities.osc != OscProfile::None,
        picker: None,
        model_config_store: None,
    };
    let between = || text_sink.finish();
    let ended = match execute(
        args,
        |_| &text_sink as &dyn EventSink,
        cancel,
        None,
        RunHost {
            driver: Driver::Interactive {
                seat: interactive_seat,
                between: &between,
            },
            elicitor: Some(elicitor),
            status: Some(status),
            diagnostics: output,
            text_output: Some(output),
            evidence: services.evidence,
        },
    )
    .await
    {
        Ok(ended) => ended,
        Err(refusal) => Ended::refused(refusal),
    };
    text_sink.finish();
    let counts = seat.exit_counts();
    output.note(&format!(
        "{} unsent {}, {} accepted {}",
        counts.drafts,
        if counts.drafts == 1 {
            "draft"
        } else {
            "drafts"
        },
        counts.accepted,
        if counts.accepted == 1 {
            "input"
        } else {
            "inputs"
        },
    ));
    announce.closing(&ended.termination);
    ended
}

/// Everything ego tells the host that is *not* the answer: the marks, the
/// progress indicator, the window title, the notification.
///
/// It lives beside the run rather than inside the sink because none of it is a
/// projection of an event. "This process started producing output" and "it
/// exited 3" are facts about the *process*, and the process is what this module
/// is. The sink publishes what happened in the session; this publishes what
/// happened to the program.
///
/// Everything here is silent unless [`Capabilities`] found a screen, so no arm
/// needs to ask again — see [`crate::render::osc`].
///
/// ## What is deliberately not here
///
/// **OSC 7 (working directory)** and **OSC 133 A (prompt start)**. Both are
/// correct for a process that *occupies* the pane and wrong for one that
/// borrows it: OSC 7 tells the terminal where the shell is, and `ego -C sub`
/// announcing `sub` would leave the pane pointing at a directory the shell
/// returns from the moment ego exits. They belong to the REPL, which is also
/// where a prompt to mark exists at all.
struct Announce<'a> {
    out: &'a dyn TextOutput,
    osc: Osc,
    quiet: bool,
    started: std::time::Instant,
}

/// How long a run has to take before finishing it is worth a desktop
/// notification.
///
/// A notification for a run the operator watched finish is an interruption
/// reporting something they already know. A minute is the point past which
/// they have plausibly looked at something else — and `-q`, which `CLI.md` §1.3
/// calls quiet progress, suppresses it at any duration.
const NOTIFY_AFTER: Duration = Duration::from_secs(60);

impl<'a> Announce<'a> {
    fn new(out: &'a dyn TextOutput, capabilities: Capabilities, quiet: bool) -> Self {
        Announce {
            out,
            osc: Osc::new(capabilities),
            quiet,
            started: std::time::Instant::now(),
        }
    }

    fn opening(&self, prompt: Option<&str>) {
        self.emit(self.preamble(prompt));
    }

    fn closing(&self, termination: &RunTermination) {
        self.emit(self.postamble(termination, self.started.elapsed()));
    }

    /// Write a sequence, or nothing at all.
    ///
    /// The empty check is not an optimisation: [`Stdout::write`] flushes, and a
    /// run in `--output json` would otherwise flush for the privilege of
    /// writing zero bytes.
    fn emit(&self, sequence: String) {
        if !sequence.is_empty() {
            self.out.write(&sequence);
        }
    }

    /// One string rather than four writes, so a pipe that breaks mid-preamble
    /// cannot leave half of an escape sequence in the stream — and so the whole
    /// announcement is a value a test can read.
    fn preamble(&self, prompt: Option<&str>) -> String {
        [
            self.osc.title(&title(prompt)),
            self.osc.state(State::Busy),
            // Indeterminate rather than a percentage: the number of turns is
            // not known in advance, and a bar that advances on a guess is a lie
            // with a progress bar's authority. `--budget` would make a fraction
            // meaningful; that is the story that gives `Progress::At` a caller.
            self.osc.progress(Progress::Indeterminate),
            self.osc.output_start(),
        ]
        .concat()
    }

    fn postamble(&self, termination: &RunTermination, elapsed: Duration) -> String {
        let code = termination.exit_code();
        let notification = if self.quiet || elapsed < NOTIFY_AFTER {
            String::new()
        } else {
            self.osc.notify(
                crate::DISPLAY_NAME,
                &format!("{} after {}", termination.exit_reason(), duration(elapsed)),
            )
        };
        [
            // Cleared on the way out on *every* path, including failure: an
            // indicator left set is a tab that stays orange until the terminal
            // is restarted, and the exit status is already carried by 133 D.
            self.osc.progress(if code == 0 {
                Progress::Cleared
            } else {
                Progress::Failed
            }),
            notification,
            self.osc.state(State::Idle),
            // Reduced to the fixed brand prefix rather than restored. Restoring
            // needs the previous title, which is only readable through a query
            // (OSC 21) that half the matrix answers and the other half echoes
            // onto the screen. `Osc::title` owns the invariant that even this
            // empty dynamic title still emits the fixed prefix; the shell
            // overwrites it at its next prompt.
            self.osc.title(""),
            // Last, because it is the mark that says everything above it
            // belongs to this command.
            self.osc.output_end(i32::from(code)),
        ]
        .concat()
    }
}

/// The window title for a run: what it was asked, short enough to read in a
/// tab.
///
/// Measured in cells rather than bytes, like everything else that reaches a
/// terminal — a title cut mid-grapheme is how a tab ends up with a replacement
/// character in it.
fn title(prompt: Option<&str>) -> String {
    const CELLS: usize = 48;
    let Some(prompt) = prompt.map(str::trim).filter(|text| !text.is_empty()) else {
        return crate::DISPLAY_NAME.to_string();
    };
    // `-` is the spelling for "read the prompt from stdin", so it is the one
    // prompt that says nothing about the run.
    if prompt == "-" {
        return crate::DISPLAY_NAME.to_string();
    }
    let first = prompt.lines().next().unwrap_or_default();
    let short = crate::render::Line::plain(first).truncate_to(CELLS);
    short.text().trim_end().to_string()
}

/// A duration a person reads, not one a machine parses.
fn duration(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    format!("{}m {}s", seconds / 60, seconds % 60)
}

/// Run `args` to completion, publishing through the sink `destination` builds.
///
/// The destination of the event stream is a *parameter* of a run, not a
/// consequence of `--output`. [`run_with_mcp_registry`] derives one from the flag because that is
/// what the flag is for; `ego acp` supplies one that turns each event into a
/// `session/update` notification, and it is the same run underneath — one
/// implementation of a turn, two places its events go.
///
/// **A function of the resolved model, not a value.** ACP's `UsageUpdate`
/// reports occupancy as a fraction of the context window, and the window is a
/// fact about the model — which is resolved *inside* this run, after the store
/// is chosen and before the first event. A caller that had to supply a ready
/// sink would have to resolve the model itself first, which is the same
/// resolution done twice with two chances to disagree. `destination` is
/// therefore called once, at the point the answer exists. A caller that does
/// not care ignores the argument: `|_| sink`.
async fn run_with_registry<S: EventSink>(
    args: Args,
    destination: impl FnOnce(&ModelCapabilities) -> S,
    cancel: CancelToken,
    evidence: Arc<EvidenceAdmissionService>,
    preflight: Option<ExecutionPreflight>,
) -> Ended {
    match execute(
        args,
        destination,
        cancel,
        preflight,
        RunHost {
            driver: Driver::Once,
            elicitor: None,
            status: None,
            diagnostics: &Stderr,
            text_output: None,
            evidence,
        },
    )
    .await
    {
        Ok(ended) => ended,
        Err(refusal) => Ended::refused(refusal),
    }
}

/// The deadline, if `--timeout` set one.
///
/// Parsed here rather than by a duration crate: `CLI.md` gives two spellings
/// ('90s', '10m') and a permissive parser would accept a third nobody
/// documented.
fn deadline(spec: &str) -> Result<Duration, UsageError> {
    crate::cli::duration_seconds(spec).map(Duration::from_secs)
}

/// The workspace this run is rooted at.
///
/// `-C DIR` is resolved once and propagated explicitly. The process working
/// directory is never mutated, so concurrent rooted adapters cannot retarget
/// one another.
pub(crate) fn workspace(directory: Option<&PathBuf>) -> Result<PathBuf, UsageError> {
    let directory = match directory {
        Some(directory) => directory.clone(),
        None => std::env::current_dir().map_err(|err| {
            UsageError(format!(
                "the working directory could not be resolved: {err}"
            ))
        })?,
    };
    let canonical = std::fs::canonicalize(&directory).map_err(|err| {
        UsageError(format!(
            "the working directory {} could not be resolved: {err}",
            directory.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(UsageError(format!(
            "the working directory {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

pub(crate) fn rooted_path(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

/// `--instructions FILE`, resolved against this workspace.
///
/// Two things happen here and neither is cosmetic.
///
/// **The directory is resolved and the file name deliberately is not.**
/// `Selection::Explicit` reaches `config::plan` verbatim, and that module
/// states its inputs must already be canonical — `root` and `cwd` are, because
/// [`workspace`] canonicalises them, and this path was not. A relative
/// `AGENTS.md` therefore reached `scope_of` with an empty parent and published
/// `scope: ""`, while an absolute one published the host's own directory into
/// a stream `InstructionDescriptor`'s doc says a CI job archives. `"."` is the
/// documented answer for both. Resolving only the parent leaves the final
/// component untouched, so a symlinked file still arrives at the loader as a
/// symlink and is still refused by name instead of quietly followed.
///
/// **A file that is not there refuses the run.** `CLI.md` §4.4: invalid config
/// is never skipped silently. The operator named this file; proceeding without
/// it ignores an instruction that was given, which is the silently vacuous
/// flag `--budget` was fixed for one flag over — and `--system @file` already
/// exits 2 for the same mistake. `symlink_metadata` rather than `exists`,
/// because a *dangling* symlink must reach the loader and be refused as the
/// symlink it is, not reported here as an absence.
fn explicit(path: &Path, cwd: &Path) -> Result<PathBuf, UsageError> {
    let refuse =
        |err: std::io::Error| UsageError(format!("--instructions {}: {err}", path.display()));
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let name = absolute.file_name().ok_or_else(|| {
        UsageError(format!(
            "--instructions {}: that is a directory, not an instruction file",
            path.display()
        ))
    })?;
    let resolved = std::fs::canonicalize(absolute.parent().unwrap_or(cwd))
        .map_err(refuse)?
        .join(name);
    std::fs::symlink_metadata(&resolved).map_err(refuse)?;
    Ok(resolved)
}

fn rooted_at_file(value: &str, cwd: &Path) -> Result<String, UsageError> {
    let Some(path) = value.strip_prefix('@') else {
        return prompt::at_file(value);
    };
    let path = Path::new(path);
    let rooted = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    prompt::at_file(&format!("@{}", rooted.display()))
}

/// The instructions and system prompt this run generates under.
///
/// **`--bare`, and a headless run in an untrusted workspace, call
/// `InstructionIo` zero times.** Not "read nothing" — make no call at all, so
/// nothing is stat'd, opened or read, and there is nothing for a dialog to be
/// raised about. That is the mechanism behind §4.4, and it is
/// `Selection::Disabled` doing it, one layer down.
fn instructions(
    effective: &Effective,
    args: &Args,
    root: &Path,
    cwd: &Path,
    diagnostics: &dyn Diagnostics,
) -> Result<Option<SystemPrompt>, UsageError> {
    if let Some(text) = &args.system {
        // `--system` replaces the prompt outright (`CLI.md` §3), so no file
        // contributed and none is published. `--append-system` still applies:
        // "append to it" means to whatever `--system` left.
        let mut text = rooted_at_file(text, cwd)?;
        if let Some(extra) = &args.append_system {
            text.push_str("\n\n");
            text.push_str(&rooted_at_file(extra, cwd)?);
        }
        return Ok(Some(SystemPrompt::verbatim(text)));
    }

    let named = match &args.instructions {
        Some(path) => Some(explicit(path, cwd)?),
        None => None,
    };
    let selection = Selection::of(effective.posture, named);
    let set = ego_core::config::load(&OsInstructionIo, root, cwd, &selection, Limits::default())
        .map_err(|err| UsageError(err.to_string()))?;

    for skipped in &set.skipped {
        diagnostics.note(&format!(
            "{} was not used: {}",
            skipped.path.display(),
            skipped.reason
        ));
    }

    let mut assembled = system_prompt(&set, root, &selection);
    if let Some(extra) = &args.append_system {
        let extra = rooted_at_file(extra, cwd)?;
        assembled = Some(match assembled {
            Some(mut prompt) => {
                prompt.text.push_str("\n\n");
                prompt.text.push_str(&extra);
                prompt
            }
            None => SystemPrompt::verbatim(extra),
        });
    }
    Ok(assembled)
}

/// The tools this run offers the model.
///
/// `--tools` names a subset and `--no-tools` names the empty one; a name that
/// is not a builtin is refused rather than ignored, because a run that
/// silently offered fewer tools than it was told to is a run whose failure has
/// no visible cause.
fn tools(
    selected: Option<&Vec<String>>,
    skills: Option<Arc<ego_core::skill::SkillCatalogue>>,
) -> Result<Vec<RegisteredTool>, UsageError> {
    let mut all: Vec<_> = builtins().into_iter().map(RegisteredTool::Effect).collect();
    all.push(RegisteredTool::HumanInput(Arc::new(AskQuestionTool)));
    if let Some(skills) = skills.filter(|catalogue| !catalogue.specs().is_empty()) {
        all.push(RegisteredTool::skill(skills));
    }
    let Some(names) = selected else {
        return Ok(all);
    };
    for name in names {
        if !all
            .iter()
            .any(|tool| tool.descriptor().name == name.as_str())
        {
            let known: Vec<String> = all.iter().map(|tool| tool.descriptor().name).collect();
            return Err(UsageError(format!(
                "--tools {name}: there is no such builtin; this build has {}",
                known.join(", ")
            )));
        }
    }
    Ok(all
        .into_iter()
        .filter(|tool| names.contains(&tool.descriptor().name))
        .collect())
}

/// The capability names `session.started` publishes.
///
/// Feature detection by name (`CLI.md` §4.2), so a consumer never parses a
/// version — and by *observation*, not by a hand-written list: a capability
/// this build does not have must not be advertised, and the cheapest way to
/// keep that true is to derive each one from the thing that provides it.
fn capabilities(tools: &[RegisteredTool], instructions: bool, schema: bool) -> Vec<String> {
    let mut names = Vec::new();
    if !tools.is_empty() {
        names.push("tools".to_string());
    }
    if instructions {
        names.push("instructions".to_string());
    }
    if schema {
        names.push("schema".to_string());
    }
    names.push("jsonl".to_string());
    names
}

/// Everything the command line and the config layers settle before this run
/// touches a store, a provider, or a file of its own.
struct Settings {
    /// `--timeout`, measured from settings resolution rather than the first
    /// turn so provider setup consumes the same absolute run budget.
    run_deadline: Option<std::time::Instant>,
    /// The workspace root, and the directory relative paths resolve against.
    /// The same path today — [`workspace`] makes the root *be* the working
    /// directory — and two fields because they answer two questions.
    root: PathBuf,
    cwd: PathBuf,
    /// The effective runtime boundary, resolved independently of permission
    /// mode and fixed for the whole run.
    confinement: RunConfinement,
    trust: Trust,
    profile_layers: Option<Vec<Sourced<ConfigLayer>>>,
    effective: Effective,
}

/// Re-resolves rule transactions against the exact config authority captured at startup.
pub(crate) struct FrozenRuleResolver {
    layers: Vec<Sourced<ConfigLayer>>,
    trust: Trust,
    selected_profile: Option<(String, Source)>,
    selection: GenerationPolicySelection,
}

impl FrozenRuleResolver {
    pub(crate) fn new(
        layers: Vec<Sourced<ConfigLayer>>,
        trust: Trust,
        selected_profile: Option<(String, Source)>,
        selection: GenerationPolicySelection,
    ) -> Self {
        Self {
            layers,
            trust,
            selected_profile,
            selection,
        }
    }

    fn resolve_rules(&self, snapshot: &RuleSnapshot) -> Result<Ruleset, PolicyControlError> {
        if snapshot.target != UserConfigTarget::Base
            || (!snapshot.permissions_present
                && (!snapshot.allow.is_empty() || !snapshot.deny.is_empty()))
        {
            return Err(PolicyControlError::Resolution {
                diagnostic: "a frozen policy resolution requires a valid base rule snapshot".into(),
            });
        }

        let mut layers = self.layers.clone();
        if let Some(user) = layers.iter_mut().find(|layer| layer.source == Source::User) {
            let captured_judge = user
                .value
                .permissions
                .as_ref()
                .and_then(|permissions| permissions.judge.clone());
            user.value.permissions = (snapshot.permissions_present || captured_judge.is_some())
                .then(|| PermCfg {
                    judge: captured_judge,
                    allow: snapshot.allow.clone(),
                    deny: snapshot.deny.clone(),
                });
        } else if snapshot.permissions_present {
            layers.push(Sourced {
                source: Source::User,
                value: ConfigLayer {
                    permissions: Some(PermCfg {
                        judge: None,
                        allow: snapshot.allow.clone(),
                        deny: snapshot.deny.clone(),
                    }),
                    ..ConfigLayer::default()
                },
            });
        }

        resolve_selected(
            &layers,
            self.trust,
            self.selected_profile
                .as_ref()
                .map(|(name, source)| (name.as_str(), *source)),
        )
        .map(|effective| effective.permissions)
        .map_err(|error| PolicyControlError::Resolution {
            diagnostic: error.to_string(),
        })
    }
}

impl FrozenPolicyResolver for FrozenRuleResolver {
    fn resolve_rule_transaction(
        &self,
        before: &RuleSnapshot,
        after: &RuleSnapshot,
        affected: &[RuleMutation],
    ) -> Result<FrozenRuleResolution, PolicyControlError> {
        if before.target != UserConfigTarget::Base
            || after.target != UserConfigTarget::Base
            || before.target != after.target
        {
            return Err(PolicyControlError::Resolution {
                diagnostic: "a frozen policy resolution requires one base rule transaction".into(),
            });
        }

        let before_rules = self.resolve_rules(before)?;
        let after_rules = self.resolve_rules(after)?;
        let stored_changed = before != after;
        let comparisons = affected
            .iter()
            .map(|mutation| {
                RuleManagement::compare_resolved(
                    &before_rules,
                    &after_rules,
                    stored_changed,
                    &mutation.rule,
                    mutation.effect,
                )
            })
            .collect();
        let mode = self.selection.next_snapshot()?.mode;
        Ok(FrozenRuleResolution {
            next: GenerationPolicySnapshot {
                mode,
                ruleset: after_rules,
            },
            comparisons,
        })
    }
}

fn handle_policy_request(
    request: PolicyControlRequest,
    mut apply: impl FnMut(PolicyControlRequest) -> Result<PolicyControlOutcome, PolicyControlError>,
    output: &dyn TextOutput,
    mut confirm_remember: impl FnMut(&RememberPreview) -> repl::Confirmation,
) -> Result<Option<repl::Line>, PolicyControlError> {
    let mismatch = || PolicyControlError::RuleManagement {
        diagnostic: "the policy-control outcome does not match its request".to_owned(),
    };
    let write_rows = |rows: Vec<String>| {
        for row in rows {
            output.line(&row);
        }
    };
    let warn = |receipt: &crate::rules::RuleMutationReceipt| {
        if let Some(warning) = crate::rules::publication_warning_text(&receipt.publication()) {
            output.note(&warning);
        }
    };

    match request {
        PolicyControlRequest::SetMode(mode) => match apply(PolicyControlRequest::SetMode(mode))? {
            PolicyControlOutcome::Mode { .. } => Ok(None),
            _ => Err(mismatch()),
        },
        PolicyControlRequest::Rule(mutation) => {
            let effect = mutation.effect;
            let PolicyControlOutcome::Rule { receipt, .. } =
                apply(PolicyControlRequest::Rule(mutation))?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_mutation_receipt_rows(&receipt, effect));
            warn(&receipt);
            Ok(None)
        }
        PolicyControlRequest::Permission(PermissionInput::List) => {
            let PolicyControlOutcome::Permission(PermissionOutcome::List(rows)) =
                apply(PolicyControlRequest::Permission(PermissionInput::List))?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_list_rows(&rows));
            Ok(None)
        }
        PolicyControlRequest::Permission(PermissionInput::Explain { call, workspace }) => {
            let PolicyControlOutcome::Permission(PermissionOutcome::Explain(explanation)) =
                apply(PolicyControlRequest::Permission(PermissionInput::Explain {
                    call: call.clone(),
                    workspace,
                }))?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_explanation_rows(&call, &explanation));
            Ok(None)
        }
        PolicyControlRequest::Permission(PermissionInput::Revoke(id)) => {
            let PolicyControlOutcome::Rule { receipt, .. } = apply(
                PolicyControlRequest::Permission(PermissionInput::Revoke(id)),
            )?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_revoke_receipt_rows(&receipt));
            warn(&receipt);
            Ok(None)
        }
        PolicyControlRequest::Permission(PermissionInput::Remember(request)) => {
            let PolicyControlOutcome::RememberPreview(preview) = apply(
                PolicyControlRequest::Permission(PermissionInput::Remember(request)),
            )?
            else {
                return Err(mismatch());
            };
            write_rows(crate::rules::render_remember_preview_rows(&preview));
            output.finish();
            match confirm_remember(&preview) {
                repl::Confirmation::Confirmed => {
                    let PolicyControlOutcome::Rule { receipt, .. } = apply(
                        PolicyControlRequest::Permission(PermissionInput::ConfirmRemember(preview)),
                    )?
                    else {
                        return Err(mismatch());
                    };
                    write_rows(crate::rules::render_remember_result_rows(
                        &UserConfigTarget::Base,
                        Some(&receipt),
                    ));
                    warn(&receipt);
                    Ok(None)
                }
                repl::Confirmation::Declined => {
                    write_rows(crate::rules::render_remember_result_rows(
                        &UserConfigTarget::Base,
                        None,
                    ));
                    Ok(None)
                }
                repl::Confirmation::Propagate(line) => Ok(Some(line)),
            }
        }
        PolicyControlRequest::Permission(PermissionInput::ConfirmRemember(_)) => Err(mismatch()),
    }
}

/// Validated direct-host facts that exist only for the explicit CI posture.
struct CiInvocation {
    session_root: PathBuf,
    artifact_root: PathBuf,
}

fn ci_invocation(
    args: &Args,
    settings: &Settings,
    driver: &Driver<'_>,
) -> Result<Option<CiInvocation>, UsageError> {
    if settings
        .effective
        .active_profile
        .as_ref()
        .is_none_or(|profile| profile.name != "ci")
    {
        return Ok(None);
    }
    if !matches!(driver, Driver::Once) {
        return Err(UsageError(
            "profile `ci` is available only through ego run or ego resume".to_owned(),
        ));
    }
    if args.instructions.is_none() {
        return Err(UsageError(
            "profile `ci` requires --instructions with an explicit file".to_owned(),
        ));
    }
    if args.prompt.is_none()
        && args.agent_file.is_none()
        && args.stdin == crate::cli::Stdin::Ambient
        && args.answers.is_empty()
    {
        return Err(UsageError(
            "profile `ci` requires a headless prompt".to_owned(),
        ));
    }

    let session_root = args
        .session_dir
        .as_ref()
        .or_else(|| {
            settings
                .effective
                .session_dir
                .as_ref()
                .map(|root| &root.value)
        })
        .ok_or_else(|| {
            UsageError(
                "profile `ci`: --session-dir must be an absolute, pre-existing directory"
                    .to_owned(),
            )
        })?;
    let artifact_root = args
        .artifact_dir
        .as_ref()
        .or_else(|| {
            settings
                .effective
                .artifact_dir
                .as_ref()
                .map(|root| &root.value)
        })
        .ok_or_else(|| {
            UsageError(
                "profile `ci`: --artifact-dir must be an absolute, pre-existing directory"
                    .to_owned(),
            )
        })?;
    let session_root = isolated_ci_root("--session-dir", session_root)?;
    let artifact_root = isolated_ci_root("--artifact-dir", artifact_root)?;
    if roots_overlap(&settings.root, &session_root)
        || roots_overlap(&settings.root, &artifact_root)
        || roots_overlap(&session_root, &artifact_root)
    {
        return Err(UsageError(
            "workspace, session, and artifact roots must be pairwise disjoint".to_owned(),
        ));
    }
    Ok(Some(CiInvocation {
        session_root,
        artifact_root,
    }))
}

fn isolated_ci_root(flag: &str, path: &Path) -> Result<PathBuf, UsageError> {
    if !path.is_absolute() {
        return Err(UsageError(format!(
            "{flag} must be an absolute, pre-existing directory"
        )));
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| {
        UsageError(format!(
            "{flag} must be an absolute, pre-existing directory"
        ))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(UsageError(format!(
            "{flag} must be an absolute, pre-existing directory whose final component is not a symlink"
        )));
    }
    std::fs::canonicalize(path).map_err(|error| {
        UsageError(format!(
            "{flag} directory {} could not be resolved: {error}",
            path.display()
        ))
    })
}

fn roots_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn ci_preflight_termination(ci: bool, termination: RunTermination) -> RunTermination {
    if !ci {
        return termination;
    }
    match termination {
        RunTermination::UsageError { message }
            if message.to_ascii_lowercase().contains("credential") =>
        {
            RunTermination::UsageError {
                message: format!("profile `ci` requires a non-interactive credential: {message}"),
            }
        }
        other => other,
    }
}

/// The command line and the config layers, resolved into one answer.
fn settings(
    args: &Args,
    trust_for: impl FnOnce(&Path) -> Trust,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    let cwd = workspace(args.directory.as_ref())?;
    rooted_settings(args, cwd, trust_for, diagnostics)
}

fn rooted_settings(
    args: &Args,
    cwd: PathBuf,
    trust_for: impl FnOnce(&Path) -> Trust,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    // The workspace root is the working directory. Discovering a repository
    // root is `git`'s question and would make the answer depend on a tool that
    // may not be installed; `-C` is how a caller says otherwise.
    let root = cwd.clone();

    // The caller can return `Trusted` only when it owns a human seat. A
    // headless caller passes a closure that always returns `Untrusted`, so no
    // terminal or stdin read is reachable from that path.
    let trust = trust_for(&root);
    let layers = config::layers(&root, args, trust)?;
    let selected_profile = config::profile(args);
    let effective = resolve_selected(
        &layers,
        trust,
        selected_profile
            .as_ref()
            .map(config::SelectedProfile::as_resolver_input),
    )
    .map_err(|err| UsageError(err.to_string()))?;
    finish_rooted_settings(args, cwd, effective, trust, Some(layers), diagnostics)
}

fn frozen_rooted_settings(
    args: &Args,
    cwd: PathBuf,
    frozen: &config::FrozenRunConfig,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    let effective = frozen.resolve(args)?;
    finish_rooted_settings(args, cwd, effective, Trust::Untrusted, None, diagnostics)
}

fn finish_rooted_settings(
    args: &Args,
    cwd: PathBuf,
    effective: Effective,
    trust: Trust,
    profile_layers: Option<Vec<Sourced<ConfigLayer>>>,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    config::report_discarded(&cwd, &effective, diagnostics);
    let started = std::time::Instant::now();
    let timeout = match args.timeout.as_deref() {
        Some(spec) => Some(deadline(spec)?),
        None => None,
    };
    let run_deadline = timeout
        .map(|timeout| {
            started
                .checked_add(timeout)
                .ok_or_else(|| UsageError("--timeout exceeds the monotonic clock".to_string()))
        })
        .transpose()?;
    let root = cwd.clone();
    if trust == Trust::Untrusted || args.bare {
        debug_assert_eq!(effective.posture, Posture::Bare);
    }

    // §4 requires the clamp be stated, not silent: a `--budget 20$` reduced to
    // an administrator's $5 must not read as binding at $20. The money half
    // travels into the gate, which publishes it on every `MeteringSelected`;
    // the token half has no such carrier, so it is said here.
    if let Some(asked) = effective.clamped.tokens {
        diagnostics.note(&format!(
            "the token ceiling of {asked} was reduced to {} by a system ceiling",
            effective
                .ceilings
                .tokens
                .expect("a clamp leaves a ceiling in force")
        ));
    }

    let confinement = RunConfinement::new(
        effective.sandbox.unwrap_or_default(),
        args.offline,
        CanonPath::new(&root),
    )
    .map_err(|err| UsageError(err.to_string()))?;

    Ok(Settings {
        run_deadline,
        root,
        cwd,
        confinement,
        trust,
        profile_layers,
        effective,
    })
}

/// What a turn is made of: the prompt, its system half, the tools on offer,
/// the schema the answer must satisfy, and the name of the model to ask.
struct Materials {
    system: Option<SystemPrompt>,
    registry: Vec<RegisteredTool>,
    skills: Option<Arc<crate::skill::SkillCatalogue>>,
    compiled: Option<schema::Schema>,
    text: String,
    slug: String,
}

/// Everything a turn needs that comes from a file, a flag or stdin.
///
/// Ordinary runs gather these before choosing a store or resolving a model.
/// Resume answers gather them only after confidential policy recovery, so
/// their filesystem, diagnostics and skill-discovery effects cannot precede
/// that recovery gate.
fn materials(
    settings: &Settings,
    args: &Args,
    seeded: bool,
    diagnostics: &dyn Diagnostics,
) -> Result<Materials, UsageError> {
    let text = if let Some(agent_file) = &args.agent_file {
        if seeded {
            prompt::seed_agent_file(agent_file.body(), args.stdin, &prompt::ProcessStdin)?
        } else {
            prompt::assemble_agent_file(agent_file.body(), args.stdin, &prompt::ProcessStdin)?
        }
    } else if seeded {
        prompt::seed(args.prompt.as_deref(), args.stdin, &prompt::ProcessStdin)?
    } else {
        prompt::assemble(args.prompt.as_deref(), args.stdin, &prompt::ProcessStdin)?
    };
    prepared_materials(settings, args, text, diagnostics)
}

fn prepared_materials(
    settings: &Settings,
    args: &Args,
    text: String,
    diagnostics: &dyn Diagnostics,
) -> Result<Materials, UsageError> {
    let schema_spec = args.schema.as_deref().map(|spec| (spec, None)).or_else(|| {
        args.agent_file
            .as_ref()
            .and_then(|file| file.schema().map(|spec| (spec, Some(file.path()))))
    });
    let compiled = match schema_spec {
        Some((spec, origin)) => {
            let source = rooted_at_file(spec, &settings.cwd).map_err(|error| match origin {
                Some(agent_file) => UsageError(format!(
                    "agent file {} selected a schema that could not be read: {error}",
                    agent_file.display()
                )),
                None => error,
            })?;
            Some(schema::Schema::compile(&source)?)
        }
        None => None,
    };
    let system = instructions(
        &settings.effective,
        args,
        &settings.root,
        &settings.cwd,
        diagnostics,
    )?;
    let skills = discover_skills(settings, diagnostics)?;
    let skill_capture = skills
        .as_ref()
        .map(|catalogue| catalogue.capture())
        .transpose()
        .map_err(|error| UsageError(error.to_string()))?;
    let registry = tools(
        args.tools.as_ref(),
        skill_capture
            .as_ref()
            .map(crate::skill::SkillCatalogueCapture::core),
    )?;
    let slug = args
        .model
        .clone()
        .or_else(|| settings.effective.model.clone().map(|model| model.0))
        .ok_or_else(|| {
            UsageError(
                "no model: pass --model provider/model, set EGO_MODEL, or put \
                 `model = \"…\"` in a config file"
                    .to_string(),
            )
        })?;

    Ok(Materials {
        system,
        registry,
        skills,
        compiled,
        text,
        slug,
    })
}

fn discover_skills(
    settings: &Settings,
    diagnostics: &dyn Diagnostics,
) -> Result<Option<Arc<crate::skill::SkillCatalogue>>, UsageError> {
    discover_skills_at(
        &settings.root,
        settings.trust,
        settings.profile_layers.as_deref(),
        diagnostics,
    )
}

fn discover_skills_at(
    root: &Path,
    trust: Trust,
    profile_layers: Option<&[Sourced<ConfigLayer>]>,
    diagnostics: &dyn Diagnostics,
) -> Result<Option<Arc<crate::skill::SkillCatalogue>>, UsageError> {
    let Some(profile_layers) = profile_layers else {
        return Ok(None);
    };
    let Some(discovery) =
        crate::skill::SkillDiscovery::for_workspace(root, trust == Trust::Trusted)
    else {
        return Ok(None);
    };
    let actions = repl::actions::ActionRegistry::final_surface();
    let fixed = actions
        .names()
        .into_iter()
        .chain(actions.aliases())
        .collect::<Vec<_>>();
    let catalogue = crate::skill::SkillCatalogue::discover_with_profile_layers(
        discovery,
        &fixed,
        profile_layers,
    )
    .map_err(|error| UsageError(error.to_string()))?;
    let capture = catalogue
        .capture()
        .map_err(|error| UsageError(error.to_string()))?;
    for diagnostic in capture.diagnostics() {
        diagnostics.note(&diagnostic.to_string());
    }
    Ok(Some(Arc::new(catalogue)))
}

/// The store this run logs through, and which session in it.
struct Chosen {
    store: JsonlStore,
    id: SessionId,
}

/// *Which* session this run continues or creates — not the opening of it.
///
/// The two halves are separated on purpose, and this is the half that happens
/// before the model resolves. Choosing is a read-only catalogue query and it
/// can refuse: `ego resume` in a directory that owns no session is a usage
/// error about the command as typed, and it would be true whatever the
/// provider said. Answering it *after* the model resolved told an operator
/// standing in the wrong directory that the network was down. Opening stays in
/// [`execute`], because foreign continuation recovery needs the replayed state.
/// The invocation-confined MCP registry reconciles before the writer admits
/// the run context.
fn chosen(args: &Args, root: &Path, store_override: Option<&Path>) -> Result<Chosen, UsageError> {
    let store_root = match store_override {
        Some(path) => path.to_path_buf(),
        None => session::store_root(args.session_dir.as_ref())?,
    };
    let store_root = rooted_path(root, store_root);
    // The same `root` that derives a named session's id below, so "the session
    // called build in this workspace" and "the sessions belonging to this
    // workspace" cannot disagree about which workspace that is.
    let store = session::jsonl(store_root, root.to_path_buf());
    let id = session::id_for(&args.session, root, &store)?;

    Ok(Chosen { store, id })
}

/// Build the fixed judge generation envelope used by both startup proofs.
///
/// Dynamic authority facts occupy one user-text block and cannot change the
/// request's tool, thinking, parallelism or output-ceiling requirements. A
/// canonical write-shaped input therefore exercises the same provider-facing
/// shape without sending a generation or pretending that the provider was
/// contacted. The exact in-turn request is checked again by `TurnLoop`.
fn fixed_judge_request(
    identity: &ModelRef,
    request_model: &str,
    max_output_tokens: Option<u64>,
    confinement: &RunConfinement,
    exec_caps: ExecCaps,
) -> GenRequest {
    let path = confinement.workspace().clone();
    let call = PreparedCall::new(
        "write",
        Op::Write {
            path: path.clone(),
            change: Change::Whole(Vec::new()),
        },
    );
    let escalation = Escalation::ModeAsks {
        mode: Mode::Auto,
        op: OpName::Write,
        path,
    };
    JudgeInput::new(&call, confinement, exec_caps, &escalation).request(&JudgeConfig {
        identity: identity.clone(),
        request_model: request_model.to_owned(),
        max_output_tokens,
    })
}

fn validate_judge_request(
    identity: &ModelRef,
    capabilities: &ModelCapabilities,
    request: &GenRequest,
    ensure_expressible: impl FnOnce(&GenRequest) -> Result<(), RequestExpressibilityError>,
) -> Result<(), String> {
    let gaps = capabilities.request_gaps(request);
    if !gaps.is_empty() {
        return Err(format!(
            "judge model `{}` is incompatible with the fixed judge request: {}",
            identity.0.as_str(),
            gaps.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    ensure_expressible(request).map_err(|error| {
        format!(
            "judge model `{}` cannot encode the fixed judge request: {error}",
            identity.0.as_str()
        )
    })
}

fn preflight_judge(
    judge: &GenerationProfile,
    confinement: &RunConfinement,
    exec_caps: ExecCaps,
) -> Result<(), RunTermination> {
    let request = fixed_judge_request(
        &judge.identity,
        &judge.request_model,
        judge.output_ceiling(),
        confinement,
        exec_caps,
    );
    validate_judge_request(&judge.identity, &judge.capabilities, &request, |request| {
        judge.generator.ensure_expressible(request)
    })
    .map_err(|message| RunTermination::UsageError { message })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CapturedAutoPreflight {
    MissingJudge,
    DeferredDynamic,
    Valid,
    Refused { diagnostic: String },
}

struct CliAutoPreflight {
    state: CapturedAutoPreflight,
}

impl CliAutoPreflight {
    fn new(state: CapturedAutoPreflight) -> Self {
        Self { state }
    }

    /// Capture only supplied request-shape facts. Dynamic facts defer to generation preflight.
    fn capture(effective: &Effective, confinement: &RunConfinement) -> Self {
        let Some(identity) = effective.judge.as_ref() else {
            return Self::new(CapturedAutoPreflight::MissingJudge);
        };
        let profile = match model::supplied_request_profile(identity) {
            Ok(Some(profile)) => profile,
            Ok(None) => return Self::new(CapturedAutoPreflight::DeferredDynamic),
            Err(error) => {
                return Self::new(CapturedAutoPreflight::Refused {
                    diagnostic: error.message,
                });
            }
        };
        let request = fixed_judge_request(
            identity,
            profile.request_model(),
            profile.output_ceiling(),
            confinement,
            ExecCaps::empty(),
        );
        let state =
            match validate_judge_request(identity, profile.capabilities(), &request, |request| {
                profile.ensure_expressible(request)
            }) {
                Ok(()) => CapturedAutoPreflight::Valid,
                Err(diagnostic) => CapturedAutoPreflight::Refused { diagnostic },
            };
        Self::new(state)
    }

    fn validate(&self) -> Result<(), String> {
        match &self.state {
            CapturedAutoPreflight::MissingJudge => {
                Err("`auto` mode requires `[permissions] judge = \"provider/model\"`".to_owned())
            }
            CapturedAutoPreflight::Refused { diagnostic } => Err(diagnostic.clone()),
            CapturedAutoPreflight::DeferredDynamic | CapturedAutoPreflight::Valid => Ok(()),
        }
    }

    fn validate_startup(&self) -> Result<(), RunTermination> {
        self.validate()
            .map_err(|message| RunTermination::UsageError { message })
    }
}

impl PreflightPort for CliAutoPreflight {
    fn validate_auto(&self) -> Result<(), PolicyControlError> {
        self.validate()
            .map_err(|diagnostic| PolicyControlError::JudgeUnavailable { diagnostic })
    }
}

fn generation_refusal(error: GenerationModelError) -> RunTermination {
    match error.class {
        GenerationModelErrorClass::Usage => RunTermination::UsageError {
            message: error.message,
        },
        GenerationModelErrorClass::Provider => RunTermination::Provider {
            message: error.message,
        },
        GenerationModelErrorClass::Internal => RunTermination::Failed {
            message: error.message,
        },
    }
}

struct GenerationPreflight {
    source: Arc<model::CliGenerationModelSource>,
    selection: Arc<model::GenerationSelection>,
    capabilities: ModelCapabilities,
    judge_available: bool,
    proof_directory: tempfile::TempDir,
    proof_evidence: SessionEvidenceStore,
}

struct GenerationPreflightInput<'a> {
    session_id: SessionId,
    confinement: &'a RunConfinement,
    effective: &'a Effective,
    slug: String,
    cancel: &'a CancelToken,
    run_deadline: Option<std::time::Instant>,
    mcp_registry: Option<Arc<McpRegistryController>>,
    native_tools: &'a [RegisteredTool],
}

async fn generation_preflight(
    input: GenerationPreflightInput<'_>,
    status: &mut Option<InteractiveStatus>,
) -> Result<GenerationPreflight, RunTermination> {
    let run_deadline = input.run_deadline;
    let cancel = input.cancel;
    let mut running = Box::pin(generation_preflight_inner(input, status));
    tokio::select! {
        biased;
        cause = stop_signal(run_deadline) => {
            cancel.cancel(cause);
            let _ = running.as_mut().await;
            Err(cancelled_termination(cancel))
        }
        result = &mut running => {
            if run_deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                cancel.cancel(CancelCause::Timeout);
            }
            if cancel.is_cancelled() {
                Err(cancelled_termination(cancel))
            } else {
                result
            }
        }
    }
}

async fn generation_preflight_inner(
    input: GenerationPreflightInput<'_>,
    status: &mut Option<InteractiveStatus>,
) -> Result<GenerationPreflight, RunTermination> {
    let GenerationPreflightInput {
        session_id,
        confinement,
        effective,
        slug,
        cancel,
        run_deadline,
        mcp_registry,
        native_tools,
    } = input;
    let proof_directory = tempfile::tempdir().map_err(|error| RunTermination::Failed {
        message: format!("could not create the sandbox proof store: {error}"),
    })?;
    let proof_evidence =
        SessionEvidenceStore::open(proof_directory.path(), session_id, EVIDENCE_CAP).map_err(
            |error| RunTermination::Failed {
                message: format!("could not open the sandbox proof store: {error}"),
            },
        )?;
    let (proof_ports, proof_observation, _) =
        effect_scope(confinement, &proof_evidence, cancel, run_deadline).await?;
    let http = Arc::new(ProviderHttpAdapter::new(proof_ports)) as Arc<dyn ego_ai::http::Http>;

    let mode = effective.mode.unwrap_or(Mode::Default);
    if mode == Mode::Auto && effective.judge.is_none() {
        return Err(RunTermination::UsageError {
            message: "`auto` mode requires `[permissions] judge = \"provider/model\"`".to_string(),
        });
    }
    let judge_identity = effective.judge.clone();
    let source = Arc::new(model::CliGenerationModelSource::new(
        ego_core::config::ModelRef(slug),
        effective.thinking,
        judge_identity,
        effective.ceilings.clone(),
        effective.auth.clone(),
    ));
    let selection = source.selection();
    let bound = source.bind(Arc::clone(&http), cancel.clone(), run_deadline);
    let main = bound
        .resolve(GenerationModelRequest::new(GenerationRole::Main))
        .await
        .map_err(generation_refusal)?;
    if status.as_ref().is_some_and(|status| status.human) {
        let catalogue_snapshot = Arc::new(
            models::build_catalogue_snapshot(
                &ModelsArgs {
                    provider: None,
                    refresh: false,
                    json: false,
                },
                &http,
            )
            .await?,
        );
        let home = config::ego_home().ok_or_else(|| RunTermination::Failed {
            message: "the model picker needs EGO_HOME or HOME for user configuration".to_string(),
        })?;
        let config_store: Arc<dyn UserConfigStore> = Arc::new(OsUserConfigStore::new(
            Arc::new(OsIo),
            home.join("config.toml"),
        ));
        let picker = Arc::new(Mutex::new(ModelPicker::for_selection(
            Arc::clone(&catalogue_snapshot),
            Arc::clone(&selection),
            Arc::clone(&config_store),
        )));
        let status = status
            .as_mut()
            .expect("a human interactive status was checked above");
        status
            .seat
            .install_model_picker(
                Arc::clone(&picker),
                Arc::clone(&selection),
                Arc::clone(&config_store),
                mcp_registry
                    .as_ref()
                    .map(Arc::clone)
                    .ok_or_else(|| RunTermination::Failed {
                        message: "interactive MCP registry authority is unavailable".to_owned(),
                    })?,
                native_tools.to_vec(),
            )
            .map_err(|error| RunTermination::Failed {
                message: format!("interactive actions could not be installed: {error}"),
            })?;
        status.picker = Some(picker);
        status.model_config_store = Some(config_store);
        debug_assert!(
            status.picker.is_some(),
            "the run retains the installed picker"
        );
    }
    let capabilities = main.capabilities.clone();
    let judge_available = effective.judge.is_some();
    if mode == Mode::Auto {
        let judge = bound
            .resolve(GenerationModelRequest::new(GenerationRole::Judge))
            .await
            .map_err(generation_refusal)?;
        preflight_judge(&judge, confinement, proof_observation.capabilities())?;
    }

    Ok(GenerationPreflight {
        source,
        selection,
        capabilities,
        judge_available,
        proof_directory,
        proof_evidence,
    })
}

/// Build the complete effect boundary for one cancellation scope.
///
/// A backend that cannot demonstrate a requested guarantee is a startup usage
/// refusal (exit 2), not an internal error and never a silent downgrade.
async fn effect_scope(
    confinement: &RunConfinement,
    evidence: &SessionEvidenceStore,
    cancel: &CancelToken,
    run_deadline: Option<std::time::Instant>,
) -> Result<
    (
        Arc<RunEffectPorts>,
        ExecObservation,
        Option<ego_core::store::evidence::EvidenceRef>,
    ),
    RunTermination,
> {
    run_effect_ports(
        confinement.clone(),
        evidence.clone(),
        cancel.clone(),
        run_deadline,
        None,
    )
    .await
    .map_err(|error| process_error_termination(error, confinement, cancel))
}

fn process_error_termination(
    error: ProcessError,
    confinement: &RunConfinement,
    cancel: &CancelToken,
) -> RunTermination {
    match &error {
        ProcessError::MissingCapabilities(missing) => RunTermination::UsageError {
            message: unmet_confinement(confinement, missing),
        },
        ProcessError::Backend(_) => RunTermination::UsageError {
            message: error.to_string(),
        },
        ProcessError::Cancelled => cancelled_termination(cancel),
        _ => RunTermination::Failed {
            message: error.to_string(),
        },
    }
}

/// The confinement backend for this build, named the way an operator would
/// look it up.
const fn confinement_backend() -> &'static str {
    if cfg!(target_os = "macos") {
        "the macOS Seatbelt backend (/usr/bin/sandbox-exec)"
    } else if cfg!(target_os = "linux") {
        "the Linux Landlock backend"
    } else {
        "no confinement backend, because this platform has none"
    }
}

/// Explain a refusal the way the person who typed the flag needs it.
///
/// `MissingExecCaps` states the measurement; it does not know which flag asked
/// for the guarantee or what would relax it. The CLI owns the sandbox flags,
/// so it owns the sentence: which mode asked, which guarantee is absent in
/// words, which backend failed to demonstrate it, and the two ways out - ask
/// for less, or fix what the recorded measurement blames.
fn unmet_confinement(confinement: &RunConfinement, missing: &MissingExecCaps) -> String {
    let absent = missing.missing();
    let mut remedies = Vec::new();
    if [
        ExecCaps::READ_SCOPED,
        ExecCaps::WRITE_DENIED,
        ExecCaps::WRITE_SCOPED,
    ]
    .into_iter()
    .any(|guarantee| absent.contains(guarantee))
    {
        remedies.push(match confinement.level() {
            Confinement::Ro => "`--sandbox off`",
            _ => "`--sandbox ro` or `--sandbox off`",
        });
    }
    if absent.contains(ExecCaps::NO_IP_NETWORK) {
        remedies.push("dropping `--offline`");
    }
    let transcript = match missing.probe_evidence() {
        Some(reference) => format!(
            " The measurement that decided it is stored as run evidence {}.",
            reference.id
        ),
        None => String::new(),
    };
    format!(
        "--sandbox {} could not be demonstrated on this host: {} showed {}, but this run needs {}.{transcript} Ask for less with {}, or fix the cause the measurement names.",
        confinement.level(),
        confinement_backend(),
        missing.available(),
        absent,
        remedies.join(", or "),
    )
}

/// The pictures one turn was given, by the two routes they can arrive on.
///
/// One value rather than two arguments because they are one question — what
/// this turn is looking at — answered by two transports: bytes that came inside
/// the request, and paths this process has yet to read. They are published
/// together, under one model's limits, or not at all.
struct Attachments<'a> {
    /// Images that arrived with the prompt itself, already decoded and bounded.
    inline: &'a [Image],
    /// `--attach` values, still to be read from the filesystem or fetched.
    specs: &'a [String],
}

impl Attachments<'_> {
    fn is_empty(&self) -> bool {
        self.inline.is_empty() && self.specs.is_empty()
    }
}

async fn acquire_attachments(
    attachments: Attachments<'_>,
    source: &Arc<model::CliGenerationModelSource>,
    ports: &Arc<RunEffectPorts>,
    cancel: &CancelToken,
    run_deadline: Option<std::time::Instant>,
    cwd: &Path,
    blobs: &dyn ego_core::store::blob::BlobStore,
) -> Result<Vec<ImageRef>, RunTermination> {
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    // Resolving the profile is what makes the refusal truthful rather than
    // generic: both admission paths need this model's own image limits, and
    // this is the first point on either where they are known.
    let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(ports))) as Arc<dyn ego_ai::http::Http>;
    let profile = source
        .bind(http, cancel.clone(), run_deadline)
        .resolve(GenerationModelRequest::new(GenerationRole::Main))
        .await
        .map_err(generation_refusal)?;
    let usage = |error: crate::cli::UsageError| RunTermination::UsageError {
        message: error.to_string(),
    };
    // Images that came inside the request come first, because they came with
    // the prompt itself; `--attach` values follow in the order they were given.
    let mut images = attachment::publish(
        attachments.inline,
        &profile.capabilities,
        profile.identity.0.as_str(),
        &profile.request_model,
        blobs,
    )
    .map_err(usage)?;
    images.extend(
        attachment::acquire(
            attachments.specs,
            &profile.capabilities,
            profile.identity.0.as_str(),
            &profile.request_model,
            cwd,
            ports.as_ref(),
            blobs,
        )
        .await
        .map_err(usage)?,
    );
    Ok(images)
}

/// Map the first latched cancellation cause to the CLI termination contract.
pub fn cancelled_termination(cancel: &CancelToken) -> RunTermination {
    match cancel.cause() {
        Some(CancelCause::Timeout) => RunTermination::TimedOut,
        Some(CancelCause::User) => RunTermination::Signalled(Signal::Int),
        _ => RunTermination::Signalled(Signal::Term),
    }
}

enum HeadlessInput<'a> {
    Prompt(&'a RunPrompt),
    Resumed(ego_core::event::id::TurnId),
    Answered {
        question: ItemId,
        answer: QuestionAnswer,
        policy: Arc<PolicyEngine>,
    },
}

enum RuntimeResume {
    Model(TurnId),
    Foreign {
        question: ItemId,
        answer: QuestionAnswer,
        policy: Arc<PolicyEngine>,
    },
}

struct SessionHookComposition<'a> {
    state: SessionHookState<'a>,
    binding: Arc<dyn HookInvocationBinding + 'a>,
    mcp: Option<Arc<dyn McpRuntimeBinding>>,
    runtime: SessionHookRuntime<'a>,
    turn: TurnId,
}

impl<'a> SessionHookComposition<'a> {
    fn bind_policy(
        &self,
        policy: Arc<PolicyEngine>,
    ) -> Result<SessionHookRuntime<'a>, RunTermination> {
        let invoker = self
            .binding
            .bind(policy, self.mcp.as_deref())
            .map_err(|message| RunTermination::Failed { message })?;
        Ok(self.state.bind(invoker))
    }

    fn bind_current(
        &self,
        source: &dyn GenerationPolicySource,
    ) -> Result<SessionHookRuntime<'a>, RunTermination> {
        let policy = source
            .current()
            .map_err(|message| RunTermination::Failed { message })?;
        self.bind_policy(policy)
    }
}

#[allow(clippy::too_many_arguments)]
async fn compose_session_hooks<'a>(
    session: SessionId,
    journal: SharedJournal<'a>,
    run_evidence: &'a dyn ego_core::evidence::EvidenceStore,
    response_evidence: &'a dyn ego_core::store::evidence::EvidenceStore,
    admission: Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    gate: SharedBudgetGate,
    layers: Option<&[Sourced<ConfigLayer>]>,
    trust: Trust,
    source: Arc<model::CliGenerationModelSource>,
    ports: Arc<RunEffectPorts>,
    policy: Arc<PolicyEngine>,
    effects: ToolEffectPorts<'a>,
    mcp_runtime: Option<Arc<dyn McpRuntimeBinding>>,
    cancel: CancelToken,
    deadline: Option<Instant>,
    instructions_loaded: bool,
) -> Result<SessionHookComposition<'a>, RunTermination> {
    let catalogue =
        HookCatalogue::admit_typed(layers.unwrap_or_default(), &[], trust).map_err(|error| {
            RunTermination::Failed {
                message: format!("the session hook catalogue is invalid: {error}"),
            }
        })?;
    let admitted = catalogue.evidence();
    let (session_start_turn, session_end_turn) = journal
        .inspect(|state| {
            let lifecycle_turn = |lifecycle| {
                let key =
                    HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Session { session });
                state
                    .hook_occurrence(&key)
                    .map(|occurrence| occurrence.turn())
            };
            (
                lifecycle_turn(HookLifecycle::SessionStart),
                lifecycle_turn(HookLifecycle::SessionEnd),
            )
        })
        .map_err(hook_journal_termination)?;
    let turn = match (session_start_turn, session_end_turn) {
        (Some(start), Some(end)) if start != end => {
            return Err(RunTermination::Failed {
                message:
                    "the durable session lifecycle occurrences disagree about their causality turn"
                        .to_owned(),
            });
        }
        (Some(turn), _) | (_, Some(turn)) => turn,
        (None, None) => TurnId::new(),
    };
    let already_admitted = journal
        .inspect(|state| state.hook_catalogue() == &admitted)
        .map_err(hook_journal_termination)?;
    if !already_admitted {
        journal
            .append(
                turn,
                None,
                None,
                EventKind::HookCatalogueAdmitted {
                    descriptors: admitted,
                },
            )
            .map_err(hook_journal_termination)?;
    }
    let durable_started = journal
        .inspect(|state| {
            state
                .hook_occurrences()
                .iter()
                .flat_map(|occurrence| {
                    occurrence.starts().iter().map(|start| {
                        HookStartedEvidence::new(
                            start.event(),
                            occurrence.event(),
                            start.hook(),
                            start.invocation(),
                            start.activation(),
                        )
                    })
                })
                .collect::<Vec<_>>()
        })
        .map_err(hook_journal_termination)?;
    let prompt = Arc::new(RunToollessGenerationService::new(
        ToollessGenerationOwner::new(
            session,
            journal.clone(),
            run_evidence,
            response_evidence,
            admission,
            invocation,
            gate,
        ),
        source,
        ports,
    ));
    let binding: Arc<dyn HookInvocationBinding + 'a> =
        Arc::new(CliHookInvocationBinding::new(effects, prompt));
    let state = SessionHookState::new(
        HookLifecycleDispatcher::new(
            HookRunner::new_with_durable_started(durable_started),
            catalogue,
        ),
        journal,
        cancel,
        deadline.unwrap_or_else(unbounded_hook_deadline),
    );
    let invoker = binding
        .bind(policy, mcp_runtime.as_deref())
        .map_err(|message| RunTermination::Failed { message })?;
    let runtime = state.bind(invoker);
    let _ = dispatch_session_hook(&runtime, session, turn, HookLifecycle::SessionStart).await?;
    if instructions_loaded {
        let _ = dispatch_session_hook(&runtime, session, turn, HookLifecycle::InstructionsLoaded)
            .await?;
    }
    Ok(SessionHookComposition {
        state,
        binding,
        mcp: mcp_runtime,
        runtime,
        turn,
    })
}

async fn dispatch_session_hook(
    runtime: &SessionHookRuntime<'_>,
    session: SessionId,
    turn: TurnId,
    lifecycle: HookLifecycle,
) -> Result<ego_core::hook::HookRunResult, RunTermination> {
    let occurrence = HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Session { session }),
        HookEvent::session(lifecycle),
        HookCausality::session(session, turn),
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    let result = runtime
        .dispatch(Producer::Main, occurrence)
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    if result.is_blocked() || result.is_observer_failure() {
        return Err(RunTermination::Failed {
            message: format!("the {} hook lifecycle did not complete", lifecycle.as_str()),
        });
    }
    Ok(result)
}

async fn dispatch_request_hook(
    runtime: &SessionHookRuntime<'_>,
    request: RequestId,
    turn: TurnId,
    parent: Option<EventId>,
    lifecycle: HookLifecycle,
) -> Result<ego_core::hook::HookRunResult, RunTermination> {
    let occurrence = HookOccurrence::new(
        HookOccurrenceKey::new(lifecycle, HookOccurrenceOwner::Request { request }),
        HookEvent::session(lifecycle),
        HookCausality::request(request, turn, parent),
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    let result = runtime
        .dispatch(Producer::Main, occurrence)
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    if result.is_blocked() || result.is_observer_failure() {
        return Err(RunTermination::Failed {
            message: format!("the {} hook lifecycle did not complete", lifecycle.as_str()),
        });
    }
    Ok(result)
}

fn hook_journal_termination(error: impl std::fmt::Display) -> RunTermination {
    RunTermination::Failed {
        message: format!("the session hook journal failed: {error}"),
    }
}

fn unbounded_hook_deadline() -> Instant {
    Instant::now()
        .checked_add(Duration::from_secs(u32::MAX.into()))
        .expect("the monotonic clock represents the process lifetime")
}

#[derive(Debug)]
struct CliChildIds;

impl IdSource for CliChildIds {
    fn next_uuid(&self) -> uuid::Uuid {
        uuid::Uuid::now_v7()
    }
}

/// The CLI's answer to everything a child run needs from its parent process.
///
/// One type where there were five, implementing one port where there were six.
/// They were never independently substitutable: the journal, the parent blob
/// store, the artifact store, the profile resolver and the selection admission
/// were composed together at exactly one call site and always all at once.
struct CliChildHost {
    store: Arc<JsonlStore>,
    session: SessionId,
    blobs: SessionEvidenceStore,
    artifacts: Arc<dyn RunArtifactStore>,
    executor: Arc<CliRunExecutor>,
    root: RunRoot,
}

impl CliChildHost {
    fn blob_error(error: impl std::fmt::Display) -> ChildResultReaderError {
        ChildResultReaderError::Read {
            diagnostic: error.to_string(),
        }
    }
}

impl ChildHost for CliChildHost {
    fn read_journal(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<Vec<ego_core::event::domain::DomainEvent>, ChildJournalError> {
        let mut stream = SessionStore::read(self.store.as_ref(), &self.session)
            .map_err(|error| ChildJournalError::new(error.to_string()))?;
        let mut events = Vec::new();
        loop {
            let entry = stream.next().now_or_never().ok_or_else(|| {
                ChildJournalError::new("the eager JSONL journal unexpectedly became asynchronous")
            })?;
            let Some(entry) = entry else {
                break;
            };
            let entry = entry.map_err(|error| ChildJournalError::new(error.to_string()))?;
            let SessionEntry::Event(event) = entry else {
                continue;
            };
            if matches!(
                &event.kind,
                EventKind::SubagentRequested { invocation: found, .. }
                    | EventKind::SubagentSpawned { invocation: found, .. }
                    | EventKind::SubagentSettled { invocation: found, .. }
                    if *found == invocation
            ) {
                events.push(event);
            }
        }
        Ok(events)
    }

    fn read_rendered_task(&self, reference: &BlobRef) -> Result<Vec<u8>, ChildResultReaderError> {
        self.blobs.read(reference).map_err(Self::blob_error)
    }

    fn read_final_assistant(
        &self,
        run: RunId,
    ) -> Result<ChildResultEvidence, ChildResultReaderError> {
        let artifact = self
            .artifacts
            .read(run, RunArtifactId::output())
            .map_err(Self::blob_error)?;
        let id = BlobId::parse(ego_core::digest::sha256_hex(artifact.bytes()))
            .map_err(Self::blob_error)?;
        Ok(ChildResultEvidence::new(
            BlobRef::new(id, artifact.bytes().len() as u64),
            artifact.bytes().to_vec(),
        ))
    }

    fn copy_to_parent(
        &self,
        _child: RunId,
        evidence: &ChildResultEvidence,
    ) -> Result<BlobRef, ChildResultReaderError> {
        let mut writer = self.blobs.begin().map_err(Self::blob_error)?;
        writer.append(evidence.bytes()).map_err(Self::blob_error)?;
        writer.commit().map_err(Self::blob_error)
    }

    fn resolve_profile(&self, name: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("child profile name is empty".to_owned());
        }
        let mut args = self.executor.args.clone();
        args.profile = Some(name.to_owned());
        args.profile_source = Some(ego_core::config::Source::Flags);
        match self.executor.frozen.as_deref() {
            Some(frozen) => frozen
                .resolve(&args)
                .map_err(|error| error.to_string())
                .and_then(|effective| {
                    self.executor
                        .admit_child_authority(&effective, args.offline)
                }),
            None => rooted_settings(
                &args,
                self.root.as_path().to_path_buf(),
                |_| Trust::Untrusted,
                &Stderr,
            )
            .map_err(|error| error.to_string())
            .and_then(|settings| {
                self.executor
                    .admit_child_authority(&settings.effective, settings.confinement.offline())
            }),
        }
    }

    fn admit_selections(
        &self,
        request: &ChildRequest,
        bindings: &RunExecutionBindings,
    ) -> Result<(RunSelections, RunExecutionBindings), String> {
        let tools = request.requested_tools().ok_or_else(|| {
            "forked skill did not supply its admitted tool intersection".to_owned()
        })?;
        Ok((
            request.selections().clone(),
            bindings.clone().with_effect_tools(tools.clone()),
        ))
    }
}

struct CliChildSinkFactory;

impl ego_core::run::EventSinkFactory for CliChildSinkFactory {
    fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }
}

struct ChildComposition {
    parent_run: RunId,
    parent_session: SessionId,
    root: RunRoot,
    store: Arc<JsonlStore>,
    executor: Arc<CliRunExecutor>,
    gate: SharedBudgetGate,
    bindings: RunExecutionBindings,
    blobs: SessionEvidenceStore,
}

fn compose_child_service(composition: ChildComposition) -> ChildRunService {
    let ChildComposition {
        parent_run,
        parent_session,
        root,
        store,
        executor,
        gate,
        bindings,
        blobs,
    } = composition;
    let controller = Arc::new(RunController::new(
        Arc::clone(&store),
        Arc::clone(&executor),
        Arc::new(SystemClock),
        Arc::new(CliChildIds),
        RunRetention::new(NonZeroUsize::new(64).expect("non-zero child retention"))
            .expect("valid child retention"),
    ));
    let artifacts: Arc<dyn RunArtifactStore> = store.clone();
    ChildRunService::new(
        controller,
        gate,
        root.clone(),
        bindings,
        ChildServicePorts::new(
            parent_run,
            Arc::new(CliChildHost {
                store,
                session: parent_session,
                blobs,
                artifacts,
                executor,
                root,
            }),
        ),
    )
}

type ChildExecutionTask = tokio::task::JoinHandle<Result<RunSnapshot, ChildServiceError>>;

struct BackgroundSkill {
    service: ChildRunService,
    invocation: SkillInvocation,
    turn: TurnId,
    execution: Option<ChildExecutionTask>,
}

struct CliSkillInvocationOwner<'a> {
    service: ChildRunService,
    hooks: SessionHookRuntime<'a>,
    parent_cancel: CancelToken,
    background: Arc<Mutex<BTreeMap<SubagentInvocationId, BackgroundSkill>>>,
}

impl<'a> CliSkillInvocationOwner<'a> {
    fn new(
        service: ChildRunService,
        hooks: SessionHookRuntime<'a>,
        parent_cancel: CancelToken,
    ) -> Self {
        Self {
            service,
            hooks,
            parent_cancel,
            background: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn launch(
        &self,
        invocation: SubagentInvocationId,
    ) -> Result<Option<ChildExecutionTask>, SkillError> {
        let Some(execution) = self
            .service
            .take_execution(invocation)
            .map_err(|error| SkillError::new(error.to_string()))?
        else {
            return Ok(None);
        };
        let service = self.service.clone();
        Ok(Some(tokio::spawn(async move {
            service.execute(execution).await
        })))
    }

    async fn await_execution(task: ChildExecutionTask) -> Result<(), SkillError> {
        task.await
            .map_err(|error| SkillError::new(format!("child execution task failed: {error}")))?
            .map(|_| ())
            .map_err(|error| SkillError::new(error.to_string()))
    }

    async fn drain(&self) -> Result<(), SkillError> {
        Self::drain_background(
            Arc::clone(&self.background),
            &self.hooks,
            self.parent_cancel.is_cancelled(),
        )
        .await
    }

    async fn drain_background(
        background: Arc<Mutex<BTreeMap<SubagentInvocationId, BackgroundSkill>>>,
        hooks: &SessionHookRuntime<'_>,
        cancel_children: bool,
    ) -> Result<(), SkillError> {
        let pending =
            std::mem::take(&mut *background.lock().expect("background skill invocations"));
        let mut first_error = None;
        for (id, background) in pending {
            let BackgroundSkill {
                service,
                invocation,
                turn,
                execution,
            } = background;
            if cancel_children && let Err(error) = service.cancel_owned(id).await {
                first_error.get_or_insert_with(|| SkillError::new(error.to_string()));
            }
            if let Some(task) = execution
                && let Err(error) = Self::await_execution(task).await
            {
                first_error.get_or_insert(error);
            }
            let source = match invocation.hook_source() {
                Ok(source) => source,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            if let Err(error) = hooks.hook_state().replace_skill_sources(turn, &[source]) {
                first_error.get_or_insert_with(|| SkillError::new(error.to_string()));
                continue;
            }
            let collected = service
                .collect_background(id, hooks)
                .await
                .map_err(|error| SkillError::new(error.to_string()));
            let cleared = hooks
                .hook_state()
                .replace_skill_sources(turn, &[])
                .map_err(|error| SkillError::new(error.to_string()));
            if let Err(error) = collected {
                first_error.get_or_insert(error);
            }
            if let Err(error) = cleared {
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl SkillInvocationPort for CliSkillInvocationOwner<'_> {
    fn invoke<'a>(
        &'a self,
        invocation: SkillInvocation,
        parent_turn: TurnId,
        parent_item: ItemId,
        parent_event: EventId,
        effect_tools: BTreeSet<String>,
    ) -> futures::future::BoxFuture<'a, Result<SkillInvocationOutcome, SkillError>> {
        Box::pin(async move {
            let Some(wait) = invocation.fork_wait() else {
                return Ok(SkillInvocationOutcome::Parent(Box::new(invocation)));
            };
            let rendered = invocation
                .rendered_ref()
                .cloned()
                .ok_or_else(|| SkillError::new("forked skill has no rendered task blob"))?;
            let thinking = invocation
                .effort()
                .map(|effort| {
                    ego_core::skill::parse_skill_effort(effort).ok_or_else(|| {
                        SkillError::new(format!(
                            "admitted child reasoning effort `{effort}` is unsupported"
                        ))
                    })
                })
                .transpose()?;
            let selections = RunSelections::new(
                invocation.model().map(|model| ModelRef(model.to_owned())),
                thinking,
                None,
                None,
                invocation.profile().map(str::to_owned),
            );
            let id = invocation.subagent_invocation();
            if self
                .service
                .recover_from_durable(
                    id,
                    RecoveryCut::BeforeParentSettlement,
                    &self.hooks,
                    wait,
                    effect_tools.clone(),
                )
                .await
                .map_err(|error| SkillError::new(error.to_string()))?
                .is_none()
            {
                let mut request = ChildRequest::new(
                    id,
                    ChildRequestOrigin::new(
                        self.service.parent_run(),
                        parent_turn,
                        parent_item,
                        parent_event,
                    ),
                    rendered,
                    selections,
                    wait,
                )
                .with_requested_tools(effect_tools);
                if let Some(profile) = invocation.profile() {
                    request = request.with_requested_profile(profile.to_owned());
                }
                self.service
                    .start(request, &self.hooks)
                    .await
                    .map_err(|error| SkillError::new(error.to_string()))?;
            }
            let execution = self.launch(id)?;
            self.background
                .lock()
                .expect("background skill invocations")
                .entry(id)
                .or_insert_with(|| BackgroundSkill {
                    service: self.service.clone(),
                    invocation: invocation.clone(),
                    turn: parent_turn,
                    execution,
                });
            match wait {
                ChildWait::Foreground => {
                    let collected = self
                        .service
                        .collect_background(id, &self.hooks)
                        .await
                        .map(SkillInvocationOutcome::Foreground)
                        .map_err(|error| SkillError::new(error.to_string()));
                    if collected.is_ok() {
                        self.background
                            .lock()
                            .expect("background skill invocations")
                            .remove(&id);
                    }
                    collected
                }
                ChildWait::Background => Ok(SkillInvocationOutcome::Background(id)),
            }
        })
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_headless_turn(
    active: &mut ActiveSession<'_>,
    opening: SessionState,
    clock: &SystemClock,
    sink: &dyn EventSink,
    cancel: &CancelToken,
    // The host-control hold this run is under, if it serves a host that can
    // ask for one. A default token for every run that does not: nothing can
    // set it, so `must_stop` never sees a hold that nobody asked for.
    pause: &PauseToken,
    source: Arc<model::CliGenerationModelSource>,
    admission: Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    parent_run: RunId,
    child_root: RunRoot,
    child_store: Arc<JsonlStore>,
    child_executor: Arc<CliRunExecutor>,
    inherited_effect_tools: Option<Arc<BTreeSet<String>>>,
    config: TurnConfig,
    gate: SharedBudgetGate,
    hook_layers: Option<&[Sourced<ConfigLayer>]>,
    hook_trust: Trust,
    mcp_runtime: Option<Arc<dyn McpRuntimeBinding>>,
    turns: &Turns<'_>,
    registry: &[RegisteredTool],
    admitted_tools: Option<Arc<dyn AdmittedToolSource>>,
    policy_source: Arc<dyn GenerationPolicySource>,
    cwd: &Path,
    confinement: &RunConfinement,
    run_deadline: Option<std::time::Instant>,
    ports: Arc<RunEffectPorts>,
    observation: ExecObservation,
    probe_evidence: Option<ego_core::store::evidence::EvidenceRef>,
    elicitor: Option<Arc<dyn Elicitor>>,
    judge_available: bool,
    attachment_specs: &[String],
    input: HeadlessInput<'_>,
    diagnostics: &dyn Diagnostics,
) -> Result<(RunTermination, SessionState, Value), RunTermination> {
    let recovered_policy = match &input {
        HeadlessInput::Answered { policy, .. } => Some(Arc::clone(policy)),
        HeadlessInput::Prompt(_) | HeadlessInput::Resumed(_) => None,
    };
    let images = acquire_attachments(
        Attachments {
            inline: match &input {
                HeadlessInput::Prompt(prompt) => prompt.images(),
                HeadlessInput::Resumed(_) | HeadlessInput::Answered { .. } => &[],
            },
            specs: attachment_specs,
        },
        &source,
        &ports,
        cancel,
        run_deadline,
        cwd,
        active.evidence().blob_store(),
    )
    .await?;
    let execution_capabilities = observation.capabilities();
    let http =
        Arc::new(ProviderHttpAdapter::new(Arc::clone(&ports))) as Arc<dyn ego_ai::http::Http>;
    let journal = SharedJournal::new(Journal::new(
        active.id(),
        active.store(),
        clock,
        sink,
        Producer::Main,
        opening,
    ));
    let instructions_loaded = config.system.is_some();
    let runtime = GenerationRuntime {
        source: source.bind(http, cancel.clone(), run_deadline),
        policy_source: Arc::clone(&policy_source),
        gate: gate.clone(),
        config,
        admission: Arc::clone(&admission),
        invocation,
    };
    let fs = OsFs;
    let effect_ports = ToolEffectPorts::new(
        &fs,
        ports.process(),
        active.evidence(),
        ports.resolver_handle(),
        ports.one_hop_handle(),
        ports.duplex_handle(),
    );
    let hook_policy = match recovered_policy.as_ref() {
        Some(policy) => Arc::clone(policy),
        None => policy_source
            .current()
            .map_err(|message| RunTermination::Failed { message })?,
    };
    let hooks = compose_session_hooks(
        active.id(),
        journal.clone(),
        active.run_evidence(),
        active.evidence(),
        admission,
        invocation,
        gate.clone(),
        hook_layers,
        hook_trust,
        Arc::clone(&source),
        Arc::clone(&ports),
        hook_policy,
        effect_ports.clone(),
        mcp_runtime.as_ref().map(Arc::clone),
        cancel.clone(),
        run_deadline,
        instructions_loaded,
    )
    .await?;
    let mut child_bindings = RunExecutionBindings::new(
        Arc::new(CliChildSinkFactory),
        elicitor.as_ref().map(Arc::clone),
    )
    .with_budget_gate(gate.clone());
    if let (Some(admitted), Some(mcp)) = (admitted_tools.as_ref(), mcp_runtime.as_ref()) {
        child_bindings = child_bindings.with_mcp(Arc::clone(admitted), Vec::new(), Arc::clone(mcp));
    }
    let child_service = compose_child_service(ChildComposition {
        parent_run,
        parent_session: active.id(),
        root: child_root,
        store: child_store,
        executor: child_executor,
        gate: gate.clone(),
        bindings: child_bindings,
        blobs: active.evidence().clone(),
    });
    let skill_owner = Arc::new(CliSkillInvocationOwner::new(
        child_service,
        hooks.runtime.clone(),
        cancel.clone(),
    ));
    let tools = match admitted_tools {
        Some(source) => {
            Tools::from_registry_with_source(registry, source, effect_ports, CanonPath::new(cwd))
                .map_err(|error| RunTermination::Failed {
                    message: format!("the admitted MCP tool catalogue is invalid: {error}"),
                })?
        }
        None => Tools::from_registry(registry, effect_ports, CanonPath::new(cwd)),
    };
    let loop_ = TurnLoop::from_shared_journal(
        active.id(),
        journal.clone(),
        active.run_evidence(),
        cancel.clone(),
        runtime,
        tools,
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?
    .with_hook_binding(
        hooks.state.clone(),
        Arc::clone(&hooks.binding),
        hooks.mcp.as_ref().map(Arc::clone),
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?
    .with_skill_invocation_port(skill_owner.clone())
    .with_pause_token(pause.clone());
    let loop_ = match recovered_policy.as_ref() {
        Some(policy) => loop_
            .with_hook_policy(Arc::clone(policy))
            .map_err(|message| RunTermination::Failed { message })?,
        None => loop_,
    };
    let loop_ = match inherited_effect_tools {
        Some(tools) => loop_.with_effect_tool_ceiling(tools),
        None => loop_,
    };
    let loop_ = match active.replayed() {
        Some(replayed) => loop_.with_ancestor_conversation(replayed.conversation().clone()),
        None => loop_,
    };
    let loop_ = match elicitor {
        Some(elicitor) => loop_.with_elicitor(elicitor),
        None => loop_,
    };
    let mut loop_ = if judge_available {
        loop_.with_judge(JudgeContext {
            confinement: confinement.clone(),
            exec_caps: execution_capabilities,
        })
    } else {
        loop_
    }
    .with_execution_observation(observation, probe_evidence);
    let (termination, result) = match input {
        HeadlessInput::Prompt(prompt) => {
            turns
                .drive_with_images(&mut loop_, prompt.text(), images, cancel, diagnostics)
                .await
        }
        HeadlessInput::Resumed(turn) => {
            turns
                .drive_resumed(&mut loop_, turn, cancel, diagnostics)
                .await
        }
        HeadlessInput::Answered {
            question, answer, ..
        } => {
            turns
                .drive_answered(&mut loop_, question, answer, cancel, diagnostics)
                .await
        }
    };
    drop(loop_);
    skill_owner
        .drain()
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    drop(skill_owner);
    let session_end_runtime = match recovered_policy {
        Some(policy) => hooks.bind_policy(policy),
        None => hooks.bind_current(policy_source.as_ref()),
    };
    let session_end = match session_end_runtime {
        Ok(runtime) => {
            dispatch_session_hook(&runtime, active.id(), hooks.turn, HookLifecycle::SessionEnd)
                .await
        }
        Err(error) => Err(error),
    };
    drop(hooks);
    let state = journal.try_into_state().map_err(hook_journal_termination)?;
    session_end?;
    Ok((termination, state, result))
}

/// Re-encode one typed selection through the current shared budget grammar.
fn run_budget_spec(budget: &Budget) -> Result<String, RunTermination> {
    match budget {
        Budget::Money(_) => Ok(budget.to_string()),
        Budget::Tokens(tokens) if tokens.is_multiple_of(1_000) => {
            Ok(format!("{}k", tokens / 1_000))
        }
        Budget::Tokens(tokens) => Err(RunTermination::UsageError {
            message: format!(
                "the typed token budget {tokens} is not representable by the current budget grammar"
            ),
        }),
    }
}

/// The headless pipeline assembled from read-only command facts before admission.
pub struct CliPrepared {
    _permit: Option<OwnedSemaphorePermit>,
    store: Arc<JsonlStore>,
    artifacts: Option<Arc<dyn RunArtifactStore>>,
    run_id: RunId,
    session_id: SessionId,
    schema_retries: u32,
    retry: u32,
    fallbacks: Vec<ModelRef>,
    max_turns: Option<u32>,
    run_deadline: Option<std::time::Instant>,
    root: PathBuf,
    cwd: PathBuf,
    confinement: RunConfinement,
    /// The exact ACP session fact to compare during effect construction.
    admitted_sandbox_observation: Option<ExecObservation>,
    effective: Effective,
    system: Option<SystemPrompt>,
    registry: Vec<RegisteredTool>,
    compiled: Option<schema::Schema>,
    slug: String,
    run_context: RunContextEvidence,
    admission: Arc<EvidenceAdmissionService>,
    child_executor: Arc<CliRunExecutor>,
}

struct PreparedExecutionRuntime {
    admission: Arc<EvidenceAdmissionService>,
    child_executor: Arc<CliRunExecutor>,
    child_authority: Option<Arc<ChildAuthorityCeiling>>,
    child: bool,
}

/// Apply what the operator explicitly chose over what configuration resolved.
///
/// Only what was chosen: inheriting a configured default is not a decision,
/// and a run that recorded it as one would claim a person picked a model they
/// never saw.
///
/// A model lands in two places because it is spelled two ways — `slug` is the
/// resolved name the preflight builds its generation target from, and
/// `effective.model` is the configuration value the rest of the turn reads.
/// Moving one without the other answers the host `model: x` and generates with
/// `y`, which is worse than refusing the pick: the host is told it landed and
/// only the evidence disagrees. They are set together here so there is one
/// place that can be got wrong instead of two that can drift.
fn apply_seat(
    state: &ego_core::control::SessionControlState,
    effective: &mut Effective,
    slug: &mut String,
) {
    if let Some(model) = state.model.chosen.clone() {
        slug.clone_from(&model.0);
        effective.model = Some(model);
    }
    if let Some(effort) = state.effort.chosen {
        effective.thinking = Some(effort);
    }
    if let Some(mode) = state.mode.chosen {
        effective.mode = Some(mode);
    }
    // `state.sandbox` is deliberately not applied here, and the omission is
    // the decision rather than an oversight. This runs at a generation
    // boundary, by which point `RunConfinement` has been built and the MCP
    // transports, the effect scope and the child authority ceiling have all
    // been derived from it. Writing `effective.sandbox` now would move
    // nothing and read as though it had. A host's pick travels instead as a
    // `RunSelections` sandbox, which lands before any of that exists, and
    // `session/set_config_option` refuses the pick outright once a run has
    // started.
}

impl CliPrepared {
    async fn prepare(
        mut args: Args,
        store: Arc<JsonlStore>,
        artifacts: Option<Arc<dyn RunArtifactStore>>,
        request: &RunPreparation,
        permit: Option<OwnedSemaphorePermit>,
        frozen: Option<&config::FrozenRunConfig>,
        runtime: PreparedExecutionRuntime,
    ) -> Result<RunPrepared<Self>, RunTermination> {
        let PreparedExecutionRuntime {
            admission,
            child_executor,
            child_authority,
            child,
        } = runtime;
        let usage = |error: UsageError| RunTermination::UsageError {
            message: error.to_string(),
        };
        if let Some(model) = request.selections.model() {
            args.model = Some(model.0.clone());
        }
        if let Some(thinking) = request.selections.thinking() {
            args.thinking = Some(thinking);
        }
        if let Some(mode) = request.selections.mode() {
            args.mode = Some(mode);
        }
        // Before `rooted_settings` below, which is the point: the confinement
        // this becomes is what the MCP admission, the effect scope and the
        // child authority ceiling are all derived from, and none of them can
        // be re-decided once they exist.
        if let Some(sandbox) = request.selections.sandbox() {
            args.sandbox = Some(sandbox);
        }
        if let Some(budget) = request.selections.budget() {
            args.budget = Some(run_budget_spec(budget)?);
        }
        if let Some(profile) = request.selections.profile() {
            args.profile = Some(profile.to_owned());
            args.profile_source = Some(ego_core::config::Source::Flags);
        }
        if args.profile.as_deref() == Some("ci") {
            return Err(RunTermination::UsageError {
                message: "profile ci available only through ego run or ego resume".to_owned(),
            });
        }
        let root = request.root.as_path().to_path_buf();
        let settings = match frozen {
            Some(frozen) => frozen_rooted_settings(&args, root, frozen, &Stderr),
            None => rooted_settings(&args, root, |_| Trust::Untrusted, &Stderr),
        }
        .map_err(usage)?;
        if let Some(authority) = child_authority.as_deref() {
            authority
                .admit(&settings.effective, settings.confinement.offline())
                .map_err(|message| RunTermination::UsageError { message })?;
        }
        let Materials {
            system,
            registry,
            skills: _,
            compiled,
            text: _,
            slug,
        } = prepared_materials(&settings, &args, String::new(), &Stderr).map_err(usage)?;
        let system = if child { None } else { system };
        let compiled = if child { None } else { compiled };
        let Settings {
            run_deadline,
            root,
            cwd,
            confinement,
            effective,
            ..
        } = settings;
        let schema_retries = args.schema_retries;
        let retry = args.retry;
        let fallbacks = args.fallbacks.clone();
        let max_turns = args.max_turns;
        let instructions = system
            .as_ref()
            .map(|prompt| prompt.sources.clone())
            .unwrap_or_default();
        let run_context = complete_run_context(
            &args,
            &effective,
            &confinement,
            &cwd,
            request.invocation,
            request.run_id,
            instructions,
        )
        .map_err(usage)?;

        Ok(RunPrepared::new(
            ModelRef(slug.clone()),
            Self {
                _permit: permit,
                store,
                artifacts,
                run_id: request.run_id,
                session_id: request.run_id.session_id(),
                schema_retries,
                retry,
                fallbacks,
                max_turns,
                run_deadline,
                root,
                cwd,
                confinement,
                admitted_sandbox_observation: request.selections.sandbox_observation().cloned(),
                effective,
                system,
                registry,
                compiled,
                slug,
                run_context,
                admission,
                child_executor,
            },
        ))
    }

    async fn submit(
        mut self,
        prompt: &RunPrompt,
        bindings: RunExecutionBindings,
        cancel: &CancelToken,
        pause: &PauseToken,
    ) -> Result<RunSettlement, RunExecutorError> {
        // The operator seat speaks before anything reads the configuration.
        //
        // A protocol session continues one run across prompts, so its
        // selections were admitted once and cannot change; the seat is the
        // only channel a live picker has. Applying it here — ahead of the
        // model preflight and the policy selection below — is what makes a
        // change land at the next generation boundary rather than at the next
        // session. Only what the operator explicitly chose is applied: an
        // inherited configuration default is not a decision to re-record.
        if let Some(controls) = bindings.controls() {
            match controls.state(self.session_id) {
                Ok(state) => apply_seat(&state, &mut self.effective, &mut self.slug),
                // An unreadable seat leaves the configured values in force.
                // Refusing the turn would let a poisoned picker lock a
                // session out of running at all.
                Err(error) => {
                    tracing::warn!(%error, "the operator seat could not be read");
                }
            }
        }
        let run_id = self.run_id;
        let artifacts = self.artifacts.clone();
        let child_root = RunRoot::new(self.root.clone())
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let child_executor = Arc::new(
            self.child_executor
                .with_child_authority(&self.effective, &self.confinement),
        );
        let clock = SystemClock;
        let opened = Opened::Durable(Box::new(
            session::open(self.store.as_ref(), &self.session_id, &clock)
                .map_err(|error| RunExecutorError::new(error.to_string()))?,
        ));
        let repair_note = opened.repair_note();
        let opening = opened.state();
        let mut active = ActiveSession::durable(opened, opening.clone(), EVIDENCE_CAP)
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let (ports, observation, probe_evidence) = match run_effect_ports(
            self.confinement.clone(),
            active.evidence().clone(),
            cancel.clone(),
            self.run_deadline,
            self.admitted_sandbox_observation.as_ref(),
        )
        .await
        {
            Ok(effects) => effects,
            Err(error @ ProcessError::ConfinementChanged { .. }) => {
                return Ok(RunSettlement::Interrupted {
                    diagnostic: Some(error.to_string()),
                });
            }
            Err(error) => {
                return settlement(process_error_termination(error, &self.confinement, cancel));
            }
        };
        if let Some(note_text) = repair_note {
            Stderr.note(&note_text);
        }
        let mut no_status = None;
        let GenerationPreflight {
            source,
            selection: _,
            capabilities: model_capabilities,
            judge_available,
            proof_directory,
            proof_evidence,
        } = generation_preflight(
            GenerationPreflightInput {
                session_id: self.session_id,
                confinement: &self.confinement,
                effective: &self.effective,
                slug: self.slug.clone(),
                cancel,
                run_deadline: self.run_deadline,
                mcp_registry: None,
                native_tools: &self.registry,
            },
            &mut no_status,
        )
        .await
        .map_err(run_executor_error)?;
        drop(proof_directory);
        drop(proof_evidence);
        let mcp_errors = bindings.mcp_errors().to_vec();
        let admitted_tools = bindings.admitted_tools();
        let mcp_runtime = bindings.mcp_runtime();
        let inherited_effect_tools = bindings.effect_tools();
        let sink = bindings.sink_factory.create(&model_capabilities);
        sink.publish(
            &PublicEvent::SessionStarted {
                v: PUBLIC_EVENT_VERSION,
                session: active.id(),
                capabilities: capabilities(
                    &self.registry,
                    self.system.is_some(),
                    self.compiled.is_some(),
                ),
                mcp_errors,
                hook_errors: Vec::new(),
                instructions: self
                    .system
                    .as_ref()
                    .map(|system| system.sources.clone())
                    .unwrap_or_default(),
            },
            &opening,
        );

        let mode = self.effective.mode.unwrap_or(Mode::Default);
        let policy_selection =
            GenerationPolicySelection::new(mode, self.effective.permissions.clone());
        // The execution context, not this process, says whether a person is
        // reachable. A terminal run hands in nothing here and an unresolved
        // permission stays exit 3; an ACP run hands in the connected host, so
        // an escalation becomes a question the operator can answer instead of
        // an internal error the host can only render as a failure.
        let policy_source: Arc<dyn GenerationPolicySource> =
            Arc::new(CliGenerationPolicySource::new(
                policy_selection,
                CanonPath::new(&self.root),
                bindings.human(),
            ));
        let config = TurnConfig {
            system: self.system,
            token_ceiling: self.effective.ceilings.tokens,
            retry: self.retry,
            fallbacks: self.fallbacks,
        };
        let turns = Turns {
            schema: self.compiled.as_ref(),
            retries: self.schema_retries,
            max_turns: self.max_turns,
            deadline: self.run_deadline,
        };
        let admission = Arc::clone(&self.admission);
        let invocation = self.run_context.invocation();
        let currency = CurrencyCode::new(SETTLEMENT).expect("a controlled currency");
        let gate = bindings.budget_gate().unwrap_or_else(|| {
            SharedBudgetGate::new(budget::gate(
                &self.effective.ceilings,
                self.effective.clamped.money,
                currency,
            ))
        });
        let (termination, state, result) = match drive_headless_turn(
            &mut active,
            opening,
            &clock,
            sink.as_ref(),
            cancel,
            pause,
            source,
            admission,
            invocation,
            run_id,
            child_root,
            Arc::clone(&self.store),
            child_executor,
            inherited_effect_tools,
            config,
            gate,
            None,
            Trust::Untrusted,
            mcp_runtime,
            &turns,
            &self.registry,
            admitted_tools,
            policy_source,
            &self.cwd,
            &self.confinement,
            self.run_deadline,
            ports,
            observation,
            probe_evidence,
            bindings.elicitor,
            judge_available,
            &[],
            HeadlessInput::Prompt(prompt),
            &Stderr,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(termination) => return settlement(termination),
        };
        active.replace_state(state);
        let settlement = settlement(termination)?;
        if matches!(&settlement, RunSettlement::Completed)
            && let Some(artifacts) = artifacts
        {
            match artifacts
                .publish(run_id, public_result_artifact(result)?)
                .map_err(|error| RunExecutorError::new(error.to_string()))?
            {
                ArtifactPublication::PublishedDurably => {}
                ArtifactPublication::PublishedDurabilityUncertain { diagnostic } => {
                    return Err(RunExecutorError::new(format!(
                        "run artifact is visible but its durability is uncertain: {diagnostic}"
                    )));
                }
            }
        }
        Ok(settlement)
    }
}

/// The exact interactive guard for the clean-session transition.
fn compaction_availability(pending_work: bool) -> repl::actions::Availability {
    if pending_work {
        repl::actions::Availability::Busy
    } else {
        repl::actions::Availability::Enabled
    }
}

enum InteractiveTurnInput {
    Prompt(String),
    Skill(AdmittedSkillOperation),
}

struct ExecutionPreflight {
    settings: Settings,
    ci: Option<CiInvocation>,
    chosen: Chosen,
    refresh: ego_mcp::McpRegistryRefresh,
    ready: Option<ExecutionReady>,
}

struct ExecutionReady {
    system: Option<SystemPrompt>,
    registry: Vec<RegisteredTool>,
    skills: Option<Arc<crate::skill::SkillCatalogue>>,
    compiled: Option<schema::Schema>,
    text: String,
    generation: GenerationReady,
}

struct GenerationReady {
    source: Arc<model::CliGenerationModelSource>,
    selection: Arc<model::GenerationSelection>,
    main_capabilities: ModelCapabilities,
    judge_available: bool,
}

/// Opaque result of the single direct-CI preflight pass.
pub struct DirectPreflight(ExecutionPreflight);

async fn select_startup_model(
    args: &Args,
    settings: &mut Settings,
    driver: &Driver<'_>,
    cancel: &CancelToken,
    diagnostics: &dyn Diagnostics,
) -> Result<(), RunTermination> {
    if args.model.is_some() || settings.effective.model.is_some() {
        return Ok(());
    }
    let home = config::ego_home();
    let observed = match home.as_ref() {
        Some(home) => crate::auth::AuthStore::new(home)
            .observed_provider_ids()
            .map_err(|error| RunTermination::UsageError {
                message: error.to_string(),
            })?,
        None => Default::default(),
    };
    let scope = crate::provider_http::CommandHttpScope::confined(
        settings.confinement.clone(),
        cancel.clone(),
        settings.run_deadline,
    )
    .await
    .map_err(|error| match error {
        crate::provider_http::McpProcessScopeError::Effects(error) => {
            process_error_termination(error, &settings.confinement, cancel)
        }
        crate::provider_http::McpProcessScopeError::Setup(message) => {
            RunTermination::Failed { message }
        }
    })?;
    let (snapshot, defaults) = models::build_startup_catalogue_snapshot(
        scope.http(),
        &crate::onboarding::ProcessEnvironment,
        &observed,
    )
    .await?;
    let usable = snapshot
        .models
        .iter()
        .filter(|entry| entry.availability == ego_ai::catalogue::ModelAvailability::Available)
        .map(|entry| entry.slug.provider.clone())
        .collect();
    let seat = match driver {
        Driver::Interactive { seat, .. } if seat.has_human() => Some(seat),
        Driver::Once | Driver::Interactive { .. } => None,
    };
    let decision = crate::onboarding::decide_missing_model(
        Arc::new(snapshot),
        &defaults,
        &usable,
        settings.effective.thinking,
        seat.is_some(),
    );
    let selection = match decision {
        crate::onboarding::StartupDecision::Headless(error) => {
            return Err(RunTermination::UsageError {
                message: error.to_string(),
            });
        }
        crate::onboarding::StartupDecision::AutoSelect(model) => (Some(model), None),
        crate::onboarding::StartupDecision::Pick(snapshot, notice) => {
            let picker = Arc::new(Mutex::new(ModelPicker::for_startup(snapshot)));
            match seat
                .expect("an interactive decision has a human seat")
                .read_startup_model(picker, notice.map(|error| error.to_string()))
            {
                crate::model_picker::PickerOutcome::ModelSwitch(request) => (None, Some(request)),
                crate::model_picker::PickerOutcome::Cancelled => {
                    cancel.cancel(CancelCause::User);
                    return Err(cancelled_termination(cancel));
                }
            }
        }
    };
    let home = home.ok_or_else(|| RunTermination::UsageError {
        message: "the startup model selection needs EGO_HOME or HOME for persistent configuration"
            .to_owned(),
    })?;
    let config_store = OsUserConfigStore::new(Arc::new(OsIo), home.join("config.toml"));
    let (publication, selected_model, selected_thinking, automatic) = match selection {
        (Some(model), None) => {
            let publication = crate::onboarding::persist_startup_model(&config_store, &model)
                .map_err(|error| RunTermination::UsageError {
                    message: format!("could not persist the startup model configuration: {error}"),
                })?;
            (publication, model, None, true)
        }
        (None, Some(request)) => {
            let publication = crate::onboarding::persist_startup_selection(&config_store, &request)
                .map_err(|error| RunTermination::UsageError {
                    message: format!("could not persist the startup model configuration: {error}"),
                })?;
            (
                publication,
                request.target(),
                Some(request.thinking()),
                false,
            )
        }
        _ => unreachable!("startup selection has exactly one source"),
    };
    if let UserConfigPublication::PublishedDurabilityUncertain { diagnostic } = publication {
        diagnostics.note(&format!(
            "model defaults are authoritative with a durability warning: {diagnostic}"
        ));
    }
    if automatic {
        diagnostics.note(&format!(
            "selected {} as the default model and saved it to user configuration",
            selected_model.0
        ));
    }
    settings.effective.model = Some(selected_model);
    if let Some(thinking) = selected_thinking {
        settings.effective.thinking = Some(thinking);
    }
    Ok(())
}

/// Complete direct-CI preflight before the process opens its MCP journals or
/// any durable session. Resume answers retain only settings and MCP selection
/// here so policy recovery can precede material and generation preparation.
pub async fn preflight_ci(
    args: &Args,
    cancel: &CancelToken,
) -> Result<DirectPreflight, RunTermination> {
    let mut status = None;
    let prepared =
        execution_preflight(args, &Driver::Once, cancel, &mut status, &Stderr, None).await?;
    debug_assert!(prepared.ci.is_some());
    Ok(DirectPreflight(prepared))
}

async fn execution_preflight(
    args: &Args,
    driver: &Driver<'_>,
    cancel: &CancelToken,
    status: &mut Option<InteractiveStatus>,
    diagnostics: &dyn Diagnostics,
    mcp_registry: Option<Arc<McpRegistryController>>,
) -> Result<ExecutionPreflight, RunTermination> {
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };
    let home = config::ego_home();
    let mut settings = settings(
        args,
        |root| run_trust(args, driver, root, home.as_deref(), diagnostics),
        diagnostics,
    )
    .map_err(usage)?;
    let ci = ci_invocation(args, &settings, driver).map_err(usage)?;
    let chosen = chosen(
        args,
        &settings.root,
        ci.as_ref().map(|ci| ci.session_root.as_path()),
    )
    .map_err(usage)?;
    if args.answers.is_empty() {
        select_startup_model(args, &mut settings, driver, cancel, diagnostics).await?;
    }
    let selected_mcp =
        config::selected_mcp(args, &settings.effective, &settings.cwd).map_err(usage)?;
    admit_mcp_transports(&settings.confinement, &selected_mcp).map_err(usage)?;
    let selected_mcp = ego_tools::mcp::materialize_admissions(selected_mcp);
    let refresh = ego_mcp::McpRegistryRefresh::new(
        selected_mcp,
        ego_mcp::RegistryTrigger::ConfigChanged,
        ego_mcp::RegistryScope::All,
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    if !args.answers.is_empty() {
        return Ok(ExecutionPreflight {
            settings,
            ci,
            chosen,
            refresh,
            ready: None,
        });
    }
    let Materials {
        system,
        registry,
        skills,
        compiled,
        text,
        slug,
    } = materials(
        &settings,
        args,
        matches!(driver, Driver::Interactive { .. }),
        diagnostics,
    )
    .map_err(usage)?;
    let GenerationPreflight {
        source,
        selection,
        capabilities: main_capabilities,
        judge_available,
        proof_directory,
        proof_evidence,
    } = generation_preflight(
        GenerationPreflightInput {
            session_id: SessionId::new(),
            confinement: &settings.confinement,
            effective: &settings.effective,
            slug,
            cancel,
            run_deadline: settings.run_deadline,
            mcp_registry,
            native_tools: &registry,
        },
        status,
    )
    .await
    .map_err(|termination| ci_preflight_termination(ci.is_some(), termination))?;
    drop(proof_directory);
    drop(proof_evidence);
    Ok(ExecutionPreflight {
        settings,
        ci,
        chosen,
        refresh,
        ready: Some(ExecutionReady {
            system,
            registry,
            skills,
            compiled,
            text,
            generation: GenerationReady {
                source,
                selection,
                main_capabilities,
                judge_available,
            },
        }),
    })
}

fn admit_mcp_transports(
    confinement: &RunConfinement,
    selected: &[(McpServerName, McpServerConfig, McpConfigProvenance)],
) -> Result<(), UsageError> {
    for (name, config, _) in selected {
        match config {
            McpServerConfig::Http(_) if !confinement.admits_mcp_http() && confinement.offline() => {
                return Err(UsageError(format!(
                    "MCP HTTP server `{name}` is unavailable under --offline"
                )));
            }
            McpServerConfig::Http(_) if !confinement.admits_mcp_http() => {
                return Err(UsageError(format!(
                    "MCP HTTP server `{name}` requires --sandbox off"
                )));
            }
            McpServerConfig::Stdio(_) if !confinement.admits_mcp_stdio() => {
                return Err(UsageError(format!(
                    "MCP stdio server `{name}` is unavailable under --offline"
                )));
            }
            McpServerConfig::Http(_) | McpServerConfig::Stdio(_) => {}
        }
    }
    Ok(())
}

/// Everything before the first turn.
///
/// What is left here after the four steps above is the *order* they happen in,
/// which is the part that is genuinely this function's: settings and explicit CI
/// posture precede the read-only session choice, then onboarding and MCP selection. A
/// direct CI token without answers already contains material and generation
/// readiness. Every prompt or resume reconciles the invocation-confined MCP
/// registry before admitting its run context. A foreign resume additionally
/// restores and pins its durable generation policy around that reconciliation.
async fn execute<S: EventSink>(
    mut args: Args,
    destination: impl FnOnce(&ModelCapabilities) -> S,
    cancel: CancelToken,
    preflight: Option<ExecutionPreflight>,
    host: RunHost<'_>,
) -> Result<Ended, RunTermination> {
    let RunHost {
        driver,
        elicitor,
        mut status,
        diagnostics,
        text_output,
        evidence,
    } = host;
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };
    let (settings, ci, refresh, ready, auto_preflight, chosen_session) = match preflight {
        Some(ExecutionPreflight {
            settings,
            ci,
            refresh,
            ready,
            chosen,
        }) => (settings, ci, refresh, ready, None, chosen),
        None => {
            let home = config::ego_home();
            let mut settings = settings(
                &args,
                |root| run_trust(&args, &driver, root, home.as_deref(), diagnostics),
                diagnostics,
            )
            .map_err(usage)?;
            let auto_preflight = Arc::new(CliAutoPreflight::capture(
                &settings.effective,
                &settings.confinement,
            ));
            let ci = ci_invocation(&args, &settings, &driver).map_err(usage)?;
            if ci.is_some() {
                return Err(RunTermination::Failed {
                    message: "direct CI execution is missing its preflight token".to_owned(),
                });
            }
            let chosen_session = chosen(&args, &settings.root, None).map_err(usage)?;
            select_startup_model(&args, &mut settings, &driver, &cancel, diagnostics).await?;
            let selected_mcp =
                config::selected_mcp(&args, &settings.effective, &settings.cwd).map_err(usage)?;
            admit_mcp_transports(&settings.confinement, &selected_mcp).map_err(usage)?;
            let selected_mcp = ego_tools::mcp::materialize_admissions(selected_mcp);
            let refresh = ego_mcp::McpRegistryRefresh::new(
                selected_mcp,
                ego_mcp::RegistryTrigger::ConfigChanged,
                ego_mcp::RegistryScope::All,
            )
            .map_err(|error| RunTermination::Failed {
                message: error.to_string(),
            })?;
            if settings.effective.mode.unwrap_or(Mode::Default) == Mode::Auto {
                auto_preflight.validate_startup()?;
            }
            (
                settings,
                ci,
                refresh,
                None,
                Some(auto_preflight),
                chosen_session,
            )
        }
    };
    if !args.quiet
        && matches!(&driver, Driver::Interactive { seat, .. } if seat.has_human())
        && let Some(model) = args.model.as_deref().or_else(|| {
            settings
                .effective
                .model
                .as_ref()
                .map(|model| model.0.as_str())
        })
    {
        diagnostics.note(&format!(
            "ego {} · model {model} · /help for commands · Alt-P for the model picker",
            env!("CARGO_PKG_VERSION")
        ));
    }
    let mcp_scope = crate::provider_http::McpProcessScope::for_run(
        settings.confinement.clone(),
        cancel.clone(),
        settings.run_deadline,
    )
    .await
    .map_err(|error| match error {
        crate::provider_http::McpProcessScopeError::Effects(error) => {
            process_error_termination(error, &settings.confinement, &cancel)
        }
        crate::provider_http::McpProcessScopeError::Setup(message) => {
            RunTermination::Failed { message }
        }
    })?;
    let mcp_runtime = Arc::clone(mcp_scope.runtime());
    let mcp_registry = Arc::clone(mcp_runtime.controller());
    if let Some(ci) = &ci {
        args.session_dir = Some(ci.session_root.clone());
    }
    let artifact_store = ci
        .as_ref()
        .map(|ci| session::jsonl(ci.artifact_root.clone(), settings.root.clone()));
    let Chosen {
        store: jsonl_store,
        id: session_id,
    } = chosen_session;
    let jsonl_store = Arc::new(jsonl_store);
    let clock = SystemClock;
    let direct_ci_ready = ready.is_some();
    let early_opened = if direct_ci_ready || !args.answers.is_empty() {
        Some(Opened::Durable(Box::new(
            session::open(jsonl_store.as_ref(), &session_id, &clock).map_err(|err| {
                RunTermination::Failed {
                    message: err.to_string(),
                }
            })?,
        )))
    } else {
        None
    };
    let mode = settings.effective.mode.unwrap_or(Mode::Default);
    // An untrusted workspace resolves to `Mode::Default`; a trusted
    // interactive run may obtain its mode from the admitted config layers.
    let human: Option<Arc<dyn HumanApprover>> = match &driver {
        Driver::Interactive { seat, .. } if seat.has_human() => Some(Arc::new(SeatApprover {
            seat: Arc::clone(seat),
        })),
        Driver::Once | Driver::Interactive { .. } => None,
    };
    let policy_selection =
        GenerationPolicySelection::new(mode, settings.effective.permissions.clone());
    let policy_source: Arc<dyn GenerationPolicySource> = Arc::new(CliGenerationPolicySource::new(
        policy_selection.clone(),
        CanonPath::new(&settings.root),
        human,
    ));
    let invocation = RunInvocationId::new();
    let run_id = RunId::new(session_id, invocation.uuid());
    let finalize = |termination| {
        finalize_ci_error(
            jsonl_store.as_ref(),
            artifact_store.as_ref(),
            run_id,
            termination,
            diagnostics,
        )
    };
    let resume_plan = if args.answers.is_empty() {
        None
    } else {
        let opened = early_opened
            .as_ref()
            .expect("a resume answer opens its session before planning");
        Some(plan_resume_answer(&opened.state(), &args.answers).map_err(&finalize)?)
    };
    let recovered_capture = match resume_plan.as_ref() {
        Some(ResumePlan::Foreign {
            continuation_id, ..
        }) => {
            let snapshot = mcp_runtime
                .load_pending_foreign_policy(continuation_id)
                .map_err(|message| finalize(RunTermination::Failed { message }))?;
            Some(
                policy_source
                    .restore(snapshot)
                    .map_err(|message| finalize(RunTermination::Failed { message }))?,
            )
        }
        Some(ResumePlan::Model { .. }) | None => None,
    };

    let (system, registry, skills, compiled, text, slug, generation) = match ready {
        Some(ExecutionReady {
            system,
            registry,
            skills,
            compiled,
            text,
            generation,
        }) => (
            system,
            registry,
            skills,
            compiled,
            text,
            None,
            Some(generation),
        ),
        None => {
            let Materials {
                system,
                registry,
                skills,
                compiled,
                text,
                slug,
            } = materials(
                &settings,
                &args,
                matches!(&driver, Driver::Interactive { .. }),
                diagnostics,
            )
            .map_err(usage)?;
            (system, registry, skills, compiled, text, Some(slug), None)
        }
    };
    let mut registry = registry;
    let skill_trust = settings.trust;
    let skill_profile_layers = settings.profile_layers.clone();
    let Settings {
        run_deadline,
        root,
        cwd,
        confinement,
        effective,
        ..
    } = settings;
    let skill_resource_roots = crate::config::ego_home().map(|user| {
        ego_core::skill::SkillResourceRoots::new(PathBuf::from("/etc/ego"), user, root.join(".ego"))
    });

    let currency = CurrencyCode::new(SETTLEMENT).expect("a constant this file controls");
    let mut opened = match early_opened {
        Some(opened) => opened,
        None => Opened::Durable(Box::new(
            session::open(jsonl_store.as_ref(), &session_id, &clock).map_err(|err| {
                RunTermination::Failed {
                    message: err.to_string(),
                }
            })?,
        )),
    };
    let child_root = RunRoot::new(root.clone()).map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    let child_artifacts: Arc<dyn RunArtifactStore> = jsonl_store.clone();
    let child_executor = Arc::new(CliRunExecutor {
        args: args.clone(),
        store: Arc::clone(&jsonl_store),
        concurrency: None,
        frozen: Some(Arc::new(
            config::FrozenRunConfig::capture(&root, &args, skill_trust).map_err(usage)?,
        )),
        artifacts: Some(child_artifacts),
        admission: Arc::clone(&evidence),
        child_authority: Some(Arc::new(ChildAuthorityCeiling::from_parent(
            &effective,
            &confinement,
        ))),
        child: true,
    });
    let instruction_descriptors = system
        .as_ref()
        .map(|prompt| prompt.sources.clone())
        .unwrap_or_default();
    let mut context = Some(
        complete_run_context(
            &args,
            &effective,
            &confinement,
            &cwd,
            invocation,
            run_id,
            instruction_descriptors,
        )
        .map_err(usage)?,
    );
    let admit_context = |opened: &Opened<'_>, context: RunContextEvidence| {
        let Opened::Durable(opened_session) = opened;
        match evidence.admit_run_context(&opened_session.writer, context) {
            Ok(EvidenceWrite::PublishedWithProjectionWarning { diagnostic }) => {
                diagnostics.note(&format!(
                    "run evidence was durable but its manifest projection failed: {diagnostic}"
                ));
                Ok(())
            }
            Ok(EvidenceWrite::Published | EvidenceWrite::AlreadyRecorded) => Ok(()),
            Err(error) => Err(RunTermination::Failed {
                message: error.to_string(),
            }),
        }
    };
    let (mut resume, foreign_plan) = match resume_plan {
        Some(ResumePlan::Model {
            answer,
            continuation,
            request_event,
        }) => {
            let continuation =
                apply_model_resume_once(&mut opened, answer, continuation, request_event)
                    .map_err(&finalize)?;
            (Some(RuntimeResume::Model(continuation.turn)), None)
        }
        Some(plan @ ResumePlan::Foreign { .. }) => (None, Some(plan)),
        None => (None, None),
    };
    mcp_registry.reconcile(refresh).await.map_err(|error| {
        finalize(RunTermination::Failed {
            message: error.to_string(),
        })
    })?;
    if recovered_capture.is_none() {
        admit_context(
            &opened,
            context
                .take()
                .expect("the run context is admitted exactly once"),
        )?;
    }
    if let Some(ResumePlan::Foreign {
        question,
        continuation_id,
        answer,
    }) = foreign_plan
    {
        let capture = recovered_capture.expect("foreign recovery loaded its policy capture");
        mcp_runtime
            .bind_pending_foreign_continuation(&continuation_id, capture.snapshot())
            .map_err(|message| finalize(RunTermination::Failed { message }))?;
        admit_context(
            &opened,
            context
                .take()
                .expect("the foreign run context is admitted after recovery"),
        )?;
        resume = Some(RuntimeResume::Foreign {
            question,
            answer,
            policy: capture.policy(),
        });
    }
    if let (Some(status), Some(skills)) = (status.as_ref(), skills.as_ref()) {
        let actions = status.seat.action_registry();
        actions
            .bind_skill_view(skills.view())
            .map_err(|error| RunTermination::Failed {
                message: format!("interactive skill actions could not be installed: {error}"),
            })?;
    }
    if let Some(note_text) = opened.repair_note() {
        diagnostics.note(&note_text);
    }
    if !text.trim().is_empty()
        && let Driver::Interactive { seat, .. } = &driver
    {
        seat.busy();
    }
    debug_assert!(context.is_none());
    let mcp_status = mcp_registry.status();
    let mcp_errors = mcp_status.mcp_errors();
    let admitted_tools = Some(mcp_registry.admitted_tools());

    let GenerationReady {
        source,
        selection,
        main_capabilities,
        judge_available,
    } = match generation {
        Some(generation) => generation,
        None => {
            let GenerationPreflight {
                source,
                selection,
                capabilities: main_capabilities,
                judge_available,
                proof_directory,
                proof_evidence,
            } = generation_preflight(
                GenerationPreflightInput {
                    session_id,
                    confinement: &confinement,
                    effective: &effective,
                    slug: slug.expect("ordinary execution prepared its model slug"),
                    cancel: &cancel,
                    run_deadline,
                    mcp_registry: Some(Arc::clone(&mcp_registry)),
                    native_tools: &registry,
                },
                &mut status,
            )
            .await
            .map_err(|termination| {
                finalize_ci_error(
                    jsonl_store.as_ref(),
                    artifact_store.as_ref(),
                    run_id,
                    ci_preflight_termination(ci.is_some(), termination),
                    diagnostics,
                )
            })?;
            drop(proof_directory);
            drop(proof_evidence);
            GenerationReady {
                source,
                selection,
                main_capabilities,
                judge_available,
            }
        }
    };

    let policy_application = match (
        status.as_ref().filter(|status| status.human),
        auto_preflight.as_ref(),
        skill_profile_layers.as_ref(),
    ) {
        (Some(status), Some(preflight), Some(layers)) => {
            status.model_config_store.as_ref().map(|store| {
                let preflight = Arc::clone(preflight) as Arc<dyn PreflightPort>;
                let resolver = Arc::new(FrozenRuleResolver::new(
                    layers.clone(),
                    skill_trust,
                    effective
                        .active_profile
                        .as_ref()
                        .map(|profile| (profile.name.clone(), profile.selected_by)),
                    policy_selection.clone(),
                ));
                Arc::new(PolicyApplicationOwner::new(
                    policy_selection.clone(),
                    Arc::clone(store),
                    preflight,
                    resolver,
                ))
            })
        }
        _ => None,
    };
    if let (Some(status), Some(_)) = (status.as_ref(), policy_application.as_ref()) {
        status.seat.enable_policy_controls();
    }

    let opening = opened.state();
    let mut active =
        ActiveSession::durable(opened, opening.clone(), EVIDENCE_CAP).map_err(|err| {
            finalize_ci_error(
                jsonl_store.as_ref(),
                artifact_store.as_ref(),
                run_id,
                RunTermination::Failed {
                    message: format!("could not open this session's tool evidence: {err}"),
                },
                diagnostics,
            )
        })?;

    // Every startup gate has passed. From here the run has a session; a later
    // fresh model-resolution refusal is recorded by the turn loop before it
    // becomes a `RunTermination`.
    let sink = destination(&main_capabilities);
    let sink = &sink;
    //
    // The opening reduction, taken once: it is what `session.started` is
    // published against and what the loop then folds into, so reading it twice
    // would let the announcement and the turn disagree about where the session
    // began.
    if let Some(status) = status.as_ref()
        && status.human
    {
        match selection.snapshot() {
            Ok(selected) => status.seat.configure_status(repl::StatusSetup {
                session: active.id(),
                state: opening.clone(),
                selection: selected,
                mode,
                cwd: cwd.clone(),
                trust: skill_trust,
                segments: effective
                    .ui
                    .statusline
                    .as_ref()
                    .and_then(|statusline| statusline.segments.clone())
                    .unwrap_or_default(),
                width: status.width,
                native: status.native,
                git: Some(Arc::new(repl::status::GitCommand::status(&cwd))),
            }),
            Err(error) => diagnostics.note(&format!(
                "the native status selection could not be read: {error}"
            )),
        }
    }
    sink.publish(
        &PublicEvent::SessionStarted {
            v: PUBLIC_EVENT_VERSION,
            session: active.id(),
            capabilities: capabilities(&registry, system.is_some(), compiled.is_some()),
            mcp_errors: mcp_errors.clone(),
            hook_errors: Vec::new(),
            instructions: system
                .as_ref()
                .map(|prompt| prompt.sources.clone())
                .unwrap_or_default(),
        },
        &opening,
    );

    let fs = OsFs;
    let gate = SharedBudgetGate::new(budget::gate(
        &effective.ceilings,
        effective.clamped.money,
        currency,
    ));
    let interactive_metadata_ceilings = effective.ceilings.clone();

    let mut config = TurnConfig {
        system,
        token_ceiling: effective.ceilings.tokens,
        retry: args.retry,
        fallbacks: args.fallbacks.clone(),
    };

    let turns = Turns {
        schema: compiled.as_ref(),
        retries: args.schema_retries,
        max_turns: args.max_turns,
        deadline: run_deadline,
    };

    // One turn-local coordinator over the session's single shared journal.
    //
    // Rebuilt per turn rather than held across the session, and that is
    // `CancelToken`'s doing rather than a preference: the token is latching
    // (`ego-core/src/turn/mod.rs:89`, "the first cause wins") and lives inside
    // `Session`, so a `Ctrl+C` on turn one would leave every later turn to
    // abort before its first check. A fresh loop takes a fresh token. It costs
    // nothing — every field below is a borrow or a clone of something already
    // assembled; generation authority itself is deliberately resolved
    // again inside each attempt.
    // A macro and not a closure: the loop borrows from this function's locals,
    // and a closure cannot name the lifetimes of a `TurnLoop<'_, '_>` it returns.
    macro_rules! build {
        ($active:expr, $cancel:expr, $ports:expr, $observation:expr, $probe_evidence:expr, $hooks:expr, $journal:expr, $skill_owner:expr) => {{
            let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(&$ports)))
                as Arc<dyn ego_ai::http::Http>;
            let runtime = GenerationRuntime {
                source: source.bind(http, $cancel.clone(), run_deadline),
                policy_source: Arc::clone(&policy_source),
                gate: gate.clone(),
                config: config.clone(),
                admission: Arc::clone(&evidence),
                invocation,
            };
            let effect_ports = ToolEffectPorts::new(
                &fs,
                $ports.process(),
                $active.evidence(),
                $ports.resolver_handle(),
                $ports.one_hop_handle(),
                $ports.duplex_handle(),
            );
            let tools = match admitted_tools.as_ref() {
                Some(source) => Tools::from_registry_with_source(
                    &registry,
                    Arc::clone(source),
                    effect_ports,
                    CanonPath::new(&cwd),
                )
                .map_err(|error| RunTermination::Failed {
                    message: format!("the admitted MCP tool catalogue is invalid: {error}"),
                })?,
                None => Tools::from_registry(&registry, effect_ports, CanonPath::new(&cwd)),
            };
            let tools = match skill_resource_roots.as_ref() {
                Some(roots) => tools.with_skill_resource_roots(roots.clone()),
                None => tools,
            };
            let loop_ = TurnLoop::from_shared_journal(
                $active.id(),
                ($journal).clone(),
                $active.run_evidence(),
                $cancel.clone(),
                runtime,
                tools,
            )
            .map_err(|error| RunTermination::Failed {
                message: error.to_string(),
            })?
            .with_hook_binding(
                ($hooks).state.clone(),
                Arc::clone(&($hooks).binding),
                ($hooks).mcp.as_ref().map(Arc::clone),
            )
            .map_err(|error| RunTermination::Failed {
                message: error.to_string(),
            })?;
            let skill_owner: Arc<dyn SkillInvocationPort + '_> = ($skill_owner).clone();
            let loop_ = loop_.with_skill_invocation_port(skill_owner);
            let loop_ = match $active.replayed() {
                Some(replayed) => loop_.with_ancestor_conversation(replayed.conversation().clone()),
                None => loop_,
            };
            let loop_ = match elicitor.as_ref() {
                Some(elicitor) => loop_.with_elicitor(Arc::clone(elicitor)),
                None => loop_,
            };
            let loop_ = if judge_available {
                loop_.with_judge(JudgeContext {
                    confinement: confinement.clone(),
                    exec_caps: $observation.capabilities(),
                })
            } else {
                loop_
            };
            loop_.with_execution_observation($observation, $probe_evidence)
        }};
    }

    macro_rules! open_session_hooks {
        ($active:expr, $state:expr, $hook_ports:ident, $policy:expr) => {{
            let hook_effects = ToolEffectPorts::new(
                &fs,
                $hook_ports.process(),
                $active.evidence(),
                $hook_ports.resolver_handle(),
                $hook_ports.one_hop_handle(),
                $hook_ports.duplex_handle(),
            );
            let leased_journal =
                LeasedSharedJournal::new($active.writer(), &clock, sink, Producer::Main, $state);
            let journal = leased_journal.shared();
            let hooks = compose_session_hooks(
                $active.id(),
                journal.clone(),
                $active.run_evidence(),
                $active.evidence(),
                Arc::clone(&evidence),
                invocation,
                gate.clone(),
                skill_profile_layers.as_deref(),
                skill_trust,
                Arc::clone(&source),
                Arc::clone(&$hook_ports),
                $policy,
                hook_effects,
                Some(Arc::clone(&mcp_runtime) as Arc<dyn McpRuntimeBinding>),
                cancel.clone(),
                run_deadline,
                config.system.is_some(),
            )
            .await?;
            (leased_journal, journal, hooks)
        }};
    }

    macro_rules! install_model_switch_owner {
        ($hooks:expr, $owner:ident, $status:expr) => {{
            $owner = ($status).as_ref().and_then(|status| {
                status.model_config_store.as_ref().map(|config_store| {
                    RunModelSwitchOwner::new(
                        Arc::clone(&selection),
                        Arc::clone(config_store),
                        ($hooks).runtime.clone(),
                        ($hooks).turn,
                        None,
                    )
                })
            });
        }};
    }

    if let Some(status) = status.as_ref()
        && status.human
    {
        status.seat.activate();
    }

    match driver {
        Driver::Once => {
            let input = match resume {
                Some(RuntimeResume::Model(turn)) => HeadlessInput::Resumed(turn),
                Some(RuntimeResume::Foreign {
                    question,
                    answer,
                    policy,
                }) => HeadlessInput::Answered {
                    question,
                    answer,
                    policy,
                },
                None => HeadlessInput::Prompt(&RunPrompt::from(text.clone())),
            };
            let (ports, observation, probe_evidence) =
                effect_scope(&confinement, active.evidence(), &cancel, run_deadline).await?;
            let driven = drive_headless_turn(
                &mut active,
                opening,
                &clock,
                sink,
                &cancel,
                &PauseToken::default(),
                Arc::clone(&source),
                Arc::clone(&evidence),
                invocation,
                run_id,
                child_root.clone(),
                Arc::clone(&jsonl_store),
                Arc::clone(&child_executor),
                None,
                config.clone(),
                gate.clone(),
                skill_profile_layers.as_deref(),
                skill_trust,
                Some(mcp_runtime.clone()),
                &turns,
                &registry,
                admitted_tools.clone(),
                Arc::clone(&policy_source),
                &cwd,
                &confinement,
                run_deadline,
                ports,
                observation,
                probe_evidence,
                elicitor,
                judge_available,
                &args.attach,
                input,
                diagnostics,
            )
            .await;
            let (termination, state, result) = match driven {
                Ok(driven) => driven,
                Err(termination) => (
                    termination,
                    active.state().clone(),
                    Value::String(String::new()),
                ),
            };
            active.replace_state(state.clone());
            let mut ended = Ended {
                termination,
                session: active.id(),
                state,
                result,
                mcp_errors: mcp_errors.clone(),
            };
            if let Some(artifact_store) = artifact_store.as_ref() {
                finalize_ci_bundle(
                    jsonl_store.as_ref(),
                    artifact_store,
                    run_id,
                    &mut ended.termination,
                    diagnostics,
                );
            }
            Ok(ended)
        }
        Driver::Interactive { seat, between } => {
            let interactive_metadata =
                SessionMetadata::new(Arc::clone(&jsonl_store), Arc::new(clock));
            type CompactionRuntime = (
                Option<SystemPrompt>,
                Vec<RegisteredTool>,
                Option<crate::skill::PreparedSkillRescan>,
            );

            enum EpochStart {
                Initial,
                New,
                Fork {
                    diagnostic: Option<String>,
                    projected: Option<Box<PublicEvent>>,
                },
                Compact {
                    diagnostic: Option<String>,
                    request: RequestId,
                    parent: Option<EventId>,
                },
            }

            enum EpochTransition<'store> {
                Exit,
                New {
                    target: ActiveSession<'store>,
                },
                Fork {
                    target: ActiveSession<'store>,
                    publication: ego_core::session::TargetPublication,
                    diagnostic: Option<String>,
                    projected: Option<PublicEvent>,
                },
                Compact {
                    target: ActiveSession<'store>,
                    publication: ego_core::session::TargetPublication,
                    diagnostic: Option<String>,
                    runtime: Option<CompactionRuntime>,
                    request: RequestId,
                    parent: Option<EventId>,
                },
            }

            let mut control = seat.take_control();
            let mut state = opening;
            let mut last = Ended {
                termination: RunTermination::Done,
                session: active.id(),
                state: state.clone(),
                result: Value::String(String::new()),
                mcp_errors: mcp_errors.clone(),
            };

            // `ego "explain this failing test"` seeds the first turn; `ego`
            // alone starts at the prompt. Both are one session — the argument
            // is a first thing to say, not a different mode.
            let mut seeded = (!text.trim().is_empty()).then(|| text.clone());
            let mut resumed = resume;
            let mut pending_attachments = args.attach.clone();
            let mut epoch_start = EpochStart::Initial;

            // Borrow-scoped session authorities live in one lexical epoch.
            // Only owned transition data crosses the boundary where the
            // replaceable ActiveSession is mutated or replaced.
            'sessions: loop {
                let (transition, source_state, session_end) = {
                    let (session_hook_ports, _, _) =
                        effect_scope(&confinement, active.evidence(), &cancel, run_deadline)
                            .await?;
                    let recovered_epoch_policy = match resumed.as_ref() {
                        Some(RuntimeResume::Foreign { policy, .. }) => Some(Arc::clone(policy)),
                        Some(RuntimeResume::Model(_)) | None => None,
                    };
                    let session_policy = match recovered_epoch_policy.as_ref() {
                        Some(policy) => Arc::clone(policy),
                        None => policy_source
                            .current()
                            .map_err(|message| RunTermination::Failed { message })?,
                    };
                    let (leased_session_journal, session_journal, session_hooks) = open_session_hooks!(
                        active,
                        active.state().clone(),
                        session_hook_ports,
                        session_policy
                    );
                    let mut child_bindings = RunExecutionBindings::new(
                        Arc::new(CliChildSinkFactory),
                        elicitor.as_ref().map(Arc::clone),
                    )
                    .with_budget_gate(gate.clone());
                    if let Some(admitted) = admitted_tools.as_ref() {
                        child_bindings = child_bindings.with_mcp(
                            Arc::clone(admitted),
                            mcp_errors.clone(),
                            Arc::clone(&mcp_runtime) as Arc<dyn McpRuntimeBinding>,
                        );
                    }
                    let child_service = compose_child_service(ChildComposition {
                        parent_run: RunId::new(active.id(), invocation.uuid()),
                        parent_session: active.id(),
                        root: child_root.clone(),
                        store: Arc::clone(&jsonl_store),
                        executor: Arc::clone(&child_executor),
                        gate: gate.clone(),
                        bindings: child_bindings,
                        blobs: active.evidence().clone(),
                    });
                    let skill_owner = Arc::new(CliSkillInvocationOwner::new(
                        child_service,
                        session_hooks.runtime.clone(),
                        cancel.clone(),
                    ));
                    let model_switch_owner;
                    install_model_switch_owner!(session_hooks, model_switch_owner, status);
                    if let EpochStart::Compact {
                        request, parent, ..
                    } = &epoch_start
                        && let Err(error) = dispatch_request_hook(
                            &session_hooks.runtime,
                            *request,
                            session_hooks.turn,
                            *parent,
                            HookLifecycle::PostCompact,
                        )
                        .await
                    {
                        diagnostics.note(&format!(
                            "the compaction post hook failed: {}",
                            error.exit_reason()
                        ));
                    }
                    state = session_hooks.state.state();
                    last.session = active.id();
                    last.state = state.clone();
                    last.result = Value::String(String::new());
                    let activation_failed = match &epoch_start {
                        EpochStart::Initial => false,
                        EpochStart::New => {
                            sink.publish(
                                &PublicEvent::SessionStarted {
                                    v: PUBLIC_EVENT_VERSION,
                                    session: active.id(),
                                    capabilities: capabilities(
                                        &registry,
                                        config.system.is_some(),
                                        compiled.is_some(),
                                    ),
                                    mcp_errors: mcp_errors.clone(),
                                    hook_errors: Vec::new(),
                                    instructions: config
                                        .system
                                        .as_ref()
                                        .map(|prompt| prompt.sources.clone())
                                        .unwrap_or_default(),
                                },
                                &state,
                            );
                            false
                        }
                        EpochStart::Fork {
                            diagnostic,
                            projected,
                        } => {
                            if let Some(diagnostic) = diagnostic {
                                diagnostics.note(&format!(
                                    "fork target {} is authoritative with a durability warning: {diagnostic}",
                                    active.id()
                                ));
                            }
                            sink.publish(
                                &PublicEvent::SessionStarted {
                                    v: PUBLIC_EVENT_VERSION,
                                    session: active.id(),
                                    capabilities: capabilities(
                                        &registry,
                                        config.system.is_some(),
                                        compiled.is_some(),
                                    ),
                                    mcp_errors: mcp_errors.clone(),
                                    hook_errors: Vec::new(),
                                    instructions: config
                                        .system
                                        .as_ref()
                                        .map(|prompt| prompt.sources.clone())
                                        .unwrap_or_default(),
                                },
                                &state,
                            );
                            if let Some(event) = projected {
                                sink.publish(event.as_ref(), &state);
                            }
                            !seat.activate_session(active.id())
                        }
                        EpochStart::Compact { diagnostic, .. } => {
                            if let Some(diagnostic) = diagnostic {
                                diagnostics.note(&format!(
                                    "compaction target is authoritative with a durability warning: {diagnostic}"
                                ));
                            }
                            sink.publish(
                                &PublicEvent::SessionStarted {
                                    v: PUBLIC_EVENT_VERSION,
                                    session: active.id(),
                                    capabilities: capabilities(
                                        &registry,
                                        config.system.is_some(),
                                        compiled.is_some(),
                                    ),
                                    mcp_errors: mcp_errors.clone(),
                                    hook_errors: Vec::new(),
                                    instructions: config
                                        .system
                                        .as_ref()
                                        .map(|prompt| prompt.sources.clone())
                                        .unwrap_or_default(),
                                },
                                &state,
                            );
                            !seat.activate_session(active.id())
                        }
                    };
                    if !activation_failed
                        && !matches!(&epoch_start, EpochStart::Initial)
                        && let Some(status) = status.as_ref()
                        && status.human
                    {
                        match selection.snapshot() {
                            Ok(selected) => status.seat.refresh_status(&state, selected),
                            Err(error) => diagnostics.note(&match &epoch_start {
                                EpochStart::New => format!(
                                    "the new session status selection could not be refreshed: {error}"
                                ),
                                EpochStart::Fork { .. } => format!(
                                    "the forked session status selection could not be refreshed: {error}"
                                ),
                                EpochStart::Compact { .. } => format!(
                                    "the compacted session status selection could not be refreshed: {error}"
                                ),
                                EpochStart::Initial => unreachable!(
                                    "initial epoch status is not refreshed through transition setup"
                                ),
                            }),
                        }
                    }
                    if activation_failed {
                        last.termination = RunTermination::Failed {
                            message: format!(
                                "session {} was published but the terminal could not activate it",
                                active.id()
                            ),
                        };
                    }
                    let transition = if activation_failed {
                        EpochTransition::Exit
                    } else {
                        'epoch: loop {
                            let resumed_turn = resumed.take();
                            let queued = !state.unacknowledged_prompts().is_empty();
                            let asked = if resumed_turn.is_some() || queued {
                                None
                            } else {
                                let queue = control.status();
                                let pending_work = queue.pending_steers > 0
                                    || queue.queued > 0
                                    || queue.interrupt_pending
                                    || !state.unacknowledged_prompts().is_empty()
                                    || state.pending_question().is_some()
                                    || !state.live_attempts.is_empty();
                                seat.set_compaction_availability(compaction_availability(
                                    pending_work,
                                ));
                                // The terminal owner keeps its next read active while this
                                // coordinator drives the turn; output returns through that
                                // same owner's bounded painter channel.
                                let line = match seeded.take() {
                                    Some(first) => repl::Line::Submitted(first),
                                    None => {
                                        seat.idle();
                                        seat.read()
                                    }
                                };

                                match line {
                                    repl::Line::Submitted(text) => {
                                        Some(InteractiveTurnInput::Prompt(text))
                                    }
                                    repl::Line::Blank => continue,
                                    // Ctrl+C at an idle prompt drops the line and nothing
                                    // else. Someone who pressed it to stop a turn must not
                                    // lose the session by pressing it once more.
                                    repl::Line::Interrupted => continue,
                                    repl::Line::Ended => break 'epoch EpochTransition::Exit,
                                    repl::Line::Refused(why) => {
                                        diagnostics.note(why);
                                        continue;
                                    }
                                    repl::Line::HostCommand(
                                        repl::HostCommand::SessionMetadata(operation),
                                    ) => {
                                        if pending_work {
                                            diagnostics.note(
                                                "session metadata is unavailable while source work is pending",
                                            );
                                            continue;
                                        }
                                        if let Err(error) = crate::metadata::execute_interactive(
                                            &interactive_metadata,
                                            &leased_session_journal,
                                            &session_journal,
                                            &operation,
                                            &interactive_metadata_ceilings,
                                            &mut |message| diagnostics.note(message),
                                        ) {
                                            diagnostics.note(&format!(
                                                "session metadata operation failed: {error}"
                                            ));
                                        }
                                        state = match session_journal.inspect(Clone::clone) {
                                            Ok(state) => state,
                                            Err(error) => {
                                                last.termination = hook_journal_termination(error);
                                                break 'epoch EpochTransition::Exit;
                                            }
                                        };
                                        last.state = state.clone();
                                        if let Some(status) = status.as_ref()
                                            && status.human
                                        {
                                            match selection.snapshot() {
                                                Ok(selected) => {
                                                    status.seat.refresh_status(&state, selected)
                                                }
                                                Err(error) => diagnostics.note(&format!(
                                                    "the metadata status selection could not be refreshed: {error}"
                                                )),
                                            }
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Skill(
                                        operation,
                                    )) => {
                                        if pending_work {
                                            diagnostics.note(
                                                "a skill cannot be invoked while source work is pending",
                                            );
                                            continue;
                                        }
                                        Some(InteractiveTurnInput::Skill(operation))
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Skills(
                                        repl::actions::SkillSurfaceAction::List,
                                    )) => {
                                        match skills.as_ref() {
                                            Some(catalogue) => match catalogue.capture() {
                                                Ok(capture) => {
                                                    diagnostics.note(&capture.render_list())
                                                }
                                                Err(error) => diagnostics.note(&format!(
                                                    "the skill catalogue could not be captured: {error}"
                                                )),
                                            },
                                            None => diagnostics
                                                .note("the skill catalogue is unavailable"),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Skills(
                                        repl::actions::SkillSurfaceAction::Diagnose,
                                    )) => {
                                        match skills.as_ref() {
                                            Some(catalogue) => match catalogue.capture() {
                                                Ok(capture) => {
                                                    diagnostics.note(&capture.render_diagnostics())
                                                }
                                                Err(error) => diagnostics.note(&format!(
                                                    "the skill catalogue could not be captured: {error}"
                                                )),
                                            },
                                            None => diagnostics
                                                .note("the skill catalogue is unavailable"),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Skills(
                                        repl::actions::SkillSurfaceAction::Rescan,
                                    )) => {
                                        if pending_work {
                                            diagnostics.note(
                                                "skills cannot be rescanned while source work is pending",
                                            );
                                            continue;
                                        }
                                        let Some(catalogue) = skills.as_ref() else {
                                            diagnostics.note("the skill catalogue is unavailable");
                                            continue;
                                        };
                                        seat.busy();
                                        let prepared = match catalogue.prepare_rescan() {
                                            Ok(prepared) => prepared,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "the skill catalogue could not be rescanned: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let published = prepared.published();
                                        let prospective = prepared.capture();
                                        debug_assert_eq!(
                                            prospective.retained_last_good(),
                                            !published
                                        );
                                        let next_registry = match tools(
                                            args.tools.as_ref(),
                                            Some(prospective.core()),
                                        ) {
                                            Ok(registry) => registry,
                                            Err(error) => {
                                                last.termination = RunTermination::Failed {
                                                    message: format!(
                                                        "the rescanned skill catalogue could not rebuild model tools: {error}"
                                                    ),
                                                };
                                                break 'epoch EpochTransition::Exit;
                                            }
                                        };
                                        let capture = match catalogue.commit_rescan(prepared) {
                                            Ok(capture) => capture,
                                            Err(error) => {
                                                last.termination = RunTermination::Failed {
                                                    message: format!(
                                                        "the prepared skill catalogue could not be published: {error}"
                                                    ),
                                                };
                                                break 'epoch EpochTransition::Exit;
                                            }
                                        };
                                        registry = next_registry;
                                        if published {
                                            diagnostics.note(&format!(
                                                "skills: published generation {}",
                                                capture.snapshot_digest()
                                            ));
                                        } else {
                                            diagnostics.note(&format!(
                                                "skills: retained last-good generation {}",
                                                capture.snapshot_digest()
                                            ));
                                            diagnostics.note(&capture.render_diagnostics());
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Hooks) => {
                                        match session_hooks.state.catalogue_snapshot() {
                                            Ok(catalogue) => match hooks::render(&catalogue, false) {
                                                Ok(rendered) if rendered.is_empty() => diagnostics
                                                    .note(&crate::output::empty_listing(
                                                        hooks::EMPTY_HOOKS,
                                                        "ego config edit",
                                                    )),
                                                Ok(rendered) => diagnostics.note(&rendered),
                                                Err(error) => diagnostics.note(&format!(
                                                    "the hook catalogue could not be rendered: {error}"
                                                )),
                                            },
                                            Err(error) => diagnostics.note(&format!(
                                                "the hook catalogue could not be captured: {error}"
                                            )),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::ModelSwitch(
                                        request,
                                    )) => {
                                        let Some(owner) = model_switch_owner.clone() else {
                                            diagnostics
                                    .note("the active session has no model-switch lifecycle owner");
                                            continue;
                                        };
                                        seat.busy();
                                        let applied = owner.apply(request).await;
                                        let publication = match &applied {
                                            Ok(publication) => publication.as_ref(),
                                            Err(error) => error.publication(),
                                        };
                                        if let Some(
                                            UserConfigPublication::PublishedDurabilityUncertain {
                                                diagnostic,
                                            },
                                        ) = publication
                                        {
                                            diagnostics.note(&format!(
                                    "model defaults are authoritative with a durability warning: {diagnostic}"
                                ));
                                        }
                                        if let Err(error) = applied {
                                            diagnostics
                                                .note(&format!("model switch failed: {error}"));
                                        }
                                        if let Some(status) = status.as_ref()
                                            && status.human
                                        {
                                            match selection.snapshot() {
                                    Ok(selected) => status.seat.refresh_status(&state, selected),
                                    Err(error) => diagnostics.note(&format!(
                                        "the model-switch status selection could not be refreshed: {error}"
                                    )),
                                }
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Policy(request)) => {
                                        let Some(owner) = policy_application.as_ref() else {
                                            let notice =
                                                "interactive policy controls are unavailable"
                                                    .to_owned();
                                            if let Some(status) = status.as_ref()
                                                && status.human
                                            {
                                                status.seat.set_notice(notice);
                                            } else {
                                                diagnostics.note(&notice);
                                            }
                                            continue;
                                        };
                                        if let PolicyControlRequest::SetMode(mode) = &request {
                                            let mode = *mode;
                                            let notice = match owner
                                                .apply(PolicyControlRequest::SetMode(mode))
                                            {
                                                Ok(PolicyControlOutcome::Mode { staging }) => {
                                                    if staging.changed {
                                                        format!(
                                                            "mode `{mode}` staged for the next generation"
                                                        )
                                                    } else {
                                                        format!("mode `{mode}` is already selected")
                                                    }
                                                }
                                                Ok(_) => "policy control failed: the outcome did not match the mode request".to_owned(),
                                                Err(error) => {
                                                    format!("policy control failed: {error}")
                                                }
                                            };
                                            if let Some(status) = status.as_ref()
                                                && status.human
                                            {
                                                status.seat.set_notice(notice);
                                            } else {
                                                diagnostics.note(&notice);
                                            }
                                            continue;
                                        }
                                        let Some(output) = text_output else {
                                            let notice =
                                                "interactive permission output is unavailable"
                                                    .to_owned();
                                            if let Some(status) = status.as_ref()
                                                && status.human
                                            {
                                                status.seat.set_notice(notice);
                                            } else {
                                                diagnostics.note(&notice);
                                            }
                                            continue;
                                        };
                                        let handled = handle_policy_request(
                                            request,
                                            |request| owner.apply(request),
                                            output,
                                            |_preview| {
                                                status
                                                    .as_ref()
                                                    .filter(|status| status.human)
                                                    .map_or(
                                                        repl::Confirmation::Declined,
                                                        |status| status.seat.confirm_remember(),
                                                    )
                                            },
                                        );
                                        match handled {
                                            Ok(Some(
                                                repl::Line::Ended
                                                | repl::Line::ExternalBreak(_),
                                            )) => break 'epoch EpochTransition::Exit,
                                            Ok(Some(_)) => diagnostics.note(
                                                "permission confirmation returned a non-terminal line",
                                            ),
                                            Ok(None) => {}
                                            Err(error) => {
                                                let notice =
                                                    format!("policy control failed: {error}");
                                                if let Some(status) = status.as_ref()
                                                    && status.human
                                                {
                                                    status.seat.set_notice(notice);
                                                } else {
                                                    diagnostics.note(&notice);
                                                }
                                            }
                                        }
                                        continue;
                                    }
                                    // The same snapshot `ego doctor` prints,
                                    // built by the same probes. Two renderings
                                    // of one value is the whole contract; two
                                    // implementations would be two diagnoses.
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Doctor,
                                    )) => {
                                        diagnostics.note(&crate::doctor::observed(root.clone()));
                                        continue;
                                    }
                                    // To the transcript rather than to a file
                                    // this action would have had to invent a
                                    // name for. `ego export ID --file` is where
                                    // a file is asked for by name; the two
                                    // disclosure modes and the three formats
                                    // are the same ones, carried by the slash
                                    // grammar — `/export local json`.
                                    repl::Line::HostCommand(repl::HostCommand::Export(
                                        selection,
                                    )) => {
                                        // The same workspace and store the
                                        // session was opened against, so
                                        // `ego --directory X` exports X and not
                                        // whatever the process happens to sit in.
                                        let args = crate::cli::ExportArgs {
                                            source: active.id(),
                                            sanitize: selection.sanitize,
                                            format: selection.format,
                                            file: None,
                                            session_dir: args.session_dir.clone(),
                                            directory: args.directory.clone(),
                                        };
                                        match exported(&args) {
                                            Ok(rendered) => diagnostics.note(&rendered),
                                            Err(termination) => diagnostics.note(&format!(
                                                "the session could not be exported: {}",
                                                termination.exit_reason()
                                            )),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Sessions,
                                    )) => {
                                        match crate::catalogue::listing(
                                            jsonl_store.as_ref(),
                                            &root,
                                            false,
                                        ) {
                                            Ok(metas) => diagnostics.note(
                                                &crate::catalogue::table(&metas, false).join("\n"),
                                            ),
                                            Err(error) => diagnostics.note(&format!(
                                                "the session catalogue could not be read: {error}"
                                            )),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Usage,
                                    )) => {
                                        let metadata = match crate::catalogue::listing(
                                            jsonl_store.as_ref(),
                                            &root,
                                            false,
                                        ) {
                                            Ok(metadata) => metadata,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                            "the session catalogue could not be read: {error}"
                                        ));
                                                continue;
                                            }
                                        };
                                        let Some(meta) = metadata
                                            .into_iter()
                                            .find(|meta| meta.id == active.id())
                                        else {
                                            diagnostics.note(
                                    "the active session is absent from its durable catalogue",
                                );
                                            continue;
                                        };
                                        match UsageReportService::new(jsonl_store.as_ref(), &clock)
                                            .report(std::slice::from_ref(&meta), UsagePeriod::All)
                                            .await
                                        {
                                            Ok(report) => {
                                                // `/usage` has no flags, so it
                                                // gets the compact default.
                                                let rendered =
                                                    crate::usage::render_text(&report, false);
                                                diagnostics.note(rendered.trim_end());
                                            }
                                            Err(error) => diagnostics.note(&format!(
                                                "the usage report could not be built: {error}"
                                            )),
                                        }
                                        continue;
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::New,
                                    )) => {
                                        if pending_work {
                                            diagnostics.note(
                                    "a new session cannot be selected while source work is pending",
                                );
                                            continue;
                                        }
                                        let target = SessionId::new();
                                        let target_opened = match session::open(
                                            jsonl_store.as_ref(),
                                            &target,
                                            &clock,
                                        ) {
                                            Ok(opened) => opened,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "the new session could not be opened: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let target_owner = match ActiveSession::durable(
                                            target_opened,
                                            SessionState::default(),
                                            EVIDENCE_CAP,
                                        ) {
                                            Ok(owner) => owner,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                        "the new session evidence could not be opened: {error}"
                                    ));
                                                continue;
                                            }
                                        };
                                        if !seat.activate_session(target) {
                                            diagnostics.note(
                                                "the terminal could not activate the new session",
                                            );
                                            continue;
                                        }
                                        break 'epoch EpochTransition::New {
                                            target: target_owner,
                                        };
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Fork,
                                    )) => {
                                        if pending_work {
                                            diagnostics.note(
                                    "the session cannot be forked while source work is pending",
                                );
                                            continue;
                                        }
                                        let prepared = match ego_core::session::PreparedFork::new(
                                            active.id(),
                                            &state,
                                            session::now(&clock),
                                        ) {
                                            Ok(prepared) => prepared,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "the session cannot be forked: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let replayed = match active.replay_seed(prepared.seed()) {
                                            Ok(replayed) => replayed,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                        "the fork source context could not be replayed: {error}"
                                    ));
                                                continue;
                                            }
                                        };
                                        let target = SessionId::new();
                                        let target_opened = match session::open(
                                            jsonl_store.as_ref(),
                                            &target,
                                            &clock,
                                        ) {
                                            Ok(opened) => opened,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "the fork target could not be opened: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let target_state =
                                            SessionState::replay([prepared.seed().event().clone()]);
                                        let mut target_owner = match ActiveSession::durable(
                                            target_opened,
                                            target_state,
                                            EVIDENCE_CAP,
                                        ) {
                                            Ok(owner) => owner,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                        "the fork target evidence could not be opened: {error}"
                                    ));
                                                continue;
                                            }
                                        };
                                        target_owner.replace_replayed(replayed);
                                        let publication =
                                            target_owner.publish_target(prepared.seed());
                                        let diagnostic =
                                            publication.diagnostic().map(str::to_string);
                                        if matches!(
                                            &publication,
                                            ego_core::session::TargetPublication::NotPublished { .. }
                                        ) {
                                            diagnostics.note(&format!(
                                                "the fork target was not published: {}",
                                                diagnostic
                                                    .as_deref()
                                                    .unwrap_or("publication was refused")
                                            ));
                                            continue;
                                        }
                                        break 'epoch EpochTransition::Fork {
                                            target: target_owner,
                                            publication,
                                            diagnostic,
                                            projected: project_public(prepared.seed().event()),
                                        };
                                    }
                                    repl::Line::HostCommand(repl::HostCommand::Action(
                                        repl::actions::ActionId::Compact,
                                    )) => {
                                        if compaction_availability(pending_work)
                                            != repl::actions::Availability::Enabled
                                        {
                                            continue;
                                        }
                                        if !seat.confirm_compaction(active.id()) {
                                            continue;
                                        }
                                        seat.busy();

                                        let target_runtime = Arc::new(Mutex::new(None));
                                        let prepared_runtime = Arc::clone(&target_runtime);
                                        let source_id = active.id();
                                        let request_id = RequestId::from_uuid(uuid::Uuid::now_v7());
                                        let pre_compact = match dispatch_request_hook(
                                            &session_hooks.runtime,
                                            request_id,
                                            session_hooks.turn,
                                            None,
                                            HookLifecycle::PreCompact,
                                        )
                                        .await
                                        {
                                            Ok(result) => result,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "compaction was blocked before preparation: {}",
                                                    error.exit_reason()
                                                ));
                                                continue;
                                            }
                                        };
                                        let compaction_cancel = CancelToken::new();
                                        let future: CompactionFuture<
                                            '_,
                                            Result<PreparedCompaction, CompactionError>,
                                        > = Box::pin(async {
                                            let target_system = instructions(
                                                &effective,
                                                &args,
                                                &root,
                                                &cwd,
                                                diagnostics,
                                            )
                                            .map_err(|error| {
                                                CompactionError::Internal(error.to_string())
                                            })?;
                                            let prepared_skills = skills
                                                .as_ref()
                                                .map(|catalogue| catalogue.prepare_rescan())
                                                .transpose()
                                                .map_err(|error| {
                                                    CompactionError::Internal(error.to_string())
                                                })?;
                                            let target_skills = prepared_skills
                                                .as_ref()
                                                .map(crate::skill::PreparedSkillRescan::capture);
                                            let target_registry = tools(
                                                args.tools.as_ref(),
                                                target_skills
                                                    .as_ref()
                                                    .map(crate::skill::SkillCatalogueCapture::core),
                                            )
                                            .map_err(|error| {
                                                CompactionError::Internal(error.to_string())
                                            })?;
                                            let target_tools: Vec<ToolDescriptor> = target_registry
                                                .iter()
                                                .map(RegisteredTool::descriptor)
                                                .collect();
                                            let (ports, observation, probe_evidence) =
                                                effect_scope(
                                                    &confinement,
                                                    active.evidence(),
                                                    &compaction_cancel,
                                                    run_deadline,
                                                )
                                                .await
                                                .map_err(compaction_termination)?;
                                            let http = Arc::new(ProviderHttpAdapter::new(
                                                Arc::clone(&ports),
                                            ))
                                                as Arc<dyn ego_ai::http::Http>;
                                            let runtime = GenerationRuntime {
                                                source: source.bind(
                                                    http,
                                                    compaction_cancel.clone(),
                                                    run_deadline,
                                                ),
                                                policy_source: Arc::clone(&policy_source),
                                                gate: gate.clone(),
                                                config: config.clone(),
                                                admission: Arc::clone(&evidence),
                                                invocation,
                                            };
                                            let effect_ports = ToolEffectPorts::new(
                                                &fs,
                                                ports.process(),
                                                active.evidence(),
                                                ports.resolver_handle(),
                                                ports.one_hop_handle(),
                                                ports.duplex_handle(),
                                            );
                                            let compaction_tools = match admitted_tools.as_ref() {
                                                Some(source) => Tools::from_registry_with_source(
                                                    &target_registry,
                                                    Arc::clone(source),
                                                    effect_ports,
                                                    CanonPath::new(&cwd),
                                                )
                                                .map_err(|error| {
                                                    CompactionError::Internal(error.to_string())
                                                })?,
                                                None => Tools::from_registry(
                                                    &target_registry,
                                                    effect_ports,
                                                    CanonPath::new(&cwd),
                                                ),
                                            };
                                            let compaction_tools =
                                                match skill_resource_roots.as_ref() {
                                                    Some(roots) => compaction_tools
                                                        .with_skill_resource_roots(roots.clone()),
                                                    None => compaction_tools,
                                                };
                                            let mut compaction = TurnLoop::from_shared_journal(
                                                active.id(),
                                                session_journal.clone(),
                                                active.run_evidence(),
                                                compaction_cancel.clone(),
                                                runtime,
                                                compaction_tools,
                                            )
                                            .map_err(|error| {
                                                CompactionError::Internal(error.to_string())
                                            })?
                                            .with_hook_binding(
                                                session_hooks.state.clone(),
                                                Arc::clone(&session_hooks.binding),
                                                session_hooks.mcp.as_ref().map(Arc::clone),
                                            )
                                            .map_err(|error| {
                                                CompactionError::Internal(error.to_string())
                                            })?;
                                            if let Some(replayed) = active.replayed() {
                                                compaction = compaction.with_ancestor_conversation(
                                                    replayed.conversation().clone(),
                                                );
                                            }
                                            if let Some(elicitor) = elicitor.as_ref() {
                                                compaction =
                                                    compaction.with_elicitor(Arc::clone(elicitor));
                                            }
                                            if judge_available {
                                                compaction = compaction.with_judge(JudgeContext {
                                                    confinement: confinement.clone(),
                                                    exec_caps: observation.capabilities(),
                                                });
                                            }
                                            let mut compaction = compaction
                                                .with_execution_observation(
                                                    observation,
                                                    probe_evidence,
                                                )
                                                .with_interactive_control(&mut control);
                                            let prepared = compaction
                                                .prepare_compaction(
                                                    target_system.clone(),
                                                    target_tools,
                                                )
                                                .await;
                                            drop(compaction);
                                            state = session_hooks.state.state();
                                            seat.refresh_queue(control.status());
                                            if !state.unacknowledged_prompts().is_empty()
                                                || state.pending_question().is_some()
                                                || !state.live_attempts.is_empty()
                                            {
                                                return Err(CompactionError::Internal(
                                        "compaction stopped before target creation because new source work became pending"
                                            .to_owned(),
                                    ));
                                            }
                                            let prepared = prepared?;
                                            *prepared_runtime
                                                .lock()
                                                .expect("interactive compaction runtime") = Some((
                                                target_system,
                                                target_registry,
                                                prepared_skills,
                                            ));
                                            Ok(prepared)
                                        });
                                        let mut preparation = DeferredCompactionPreparation {
                                            source: source_id,
                                            future: Some(future),
                                        };
                                        let request = CompactionTransitionRequest {
                                            request_id,
                                            source: source_id,
                                        };
                                        let receipt = CompactionTransitionService::new(clock)
                                            .compact(
                                                request,
                                                &mut preparation,
                                                jsonl_store.as_ref(),
                                            )
                                            .await;
                                        drop(preparation);
                                        let receipt = match receipt {
                                            Ok(receipt) => receipt,
                                            Err(error) => {
                                                diagnostics
                                                    .note(&format!("compaction failed: {error}"));
                                                if let Some(status) = status.as_ref()
                                                    && status.human
                                                    && let Ok(selected) = selection.snapshot()
                                                {
                                                    status.seat.refresh_status(&state, selected);
                                                }
                                                continue;
                                            }
                                        };
                                        let target = receipt.target;
                                        let target_opened = match session::open(
                                            jsonl_store.as_ref(),
                                            &target,
                                            &clock,
                                        ) {
                                            Ok(opened) => Opened::Durable(Box::new(opened)),
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                                    "compaction target could not be opened: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let target_state = target_opened.state();
                                        let target_owner = match ActiveSession::durable(
                                            target_opened,
                                            target_state,
                                            EVIDENCE_CAP,
                                        ) {
                                            Ok(owner) => owner,
                                            Err(error) => {
                                                diagnostics.note(&format!(
                                        "compaction target evidence could not be opened: {error}"
                                    ));
                                                continue;
                                            }
                                        };
                                        let diagnostic =
                                            receipt.publication.diagnostic().map(str::to_string);
                                        if matches!(
                                            &receipt.publication,
                                            ego_core::session::TargetPublication::NotPublished { .. }
                                        ) {
                                            diagnostics.note("compaction target was not published");
                                            continue;
                                        }
                                        let runtime = target_runtime
                                            .lock()
                                            .expect("interactive compaction runtime")
                                            .take();
                                        break 'epoch EpochTransition::Compact {
                                            target: target_owner,
                                            publication: receipt.publication,
                                            diagnostic,
                                            runtime,
                                            request: request_id,
                                            parent: pre_compact.closure_event(),
                                        };
                                    }
                                    repl::Line::HostCommand(_) => continue,
                                    repl::Line::ExternalBreak(_) => {
                                        break 'epoch EpochTransition::Exit;
                                    }
                                }
                            };

                            // Per turn, for the reason `build` documents.
                            seat.busy();
                            let turn_cancel = CancelToken::new();
                            let (ports, observation, probe_evidence) = effect_scope(
                                &confinement,
                                active.evidence(),
                                &turn_cancel,
                                run_deadline,
                            )
                            .await?;
                            let images =
                                if matches!(asked.as_ref(), Some(InteractiveTurnInput::Prompt(_)))
                                    && !pending_attachments.is_empty()
                                {
                                    let images = acquire_attachments(
                                        Attachments {
                                            // The interactive surface attaches
                                            // by spec; a typed prompt carries
                                            // no inline bytes of its own.
                                            inline: &[],
                                            specs: &pending_attachments,
                                        },
                                        &source,
                                        &ports,
                                        &turn_cancel,
                                        run_deadline,
                                        &cwd,
                                        active.evidence().blob_store(),
                                    )
                                    .await?;
                                    pending_attachments.clear();
                                    images
                                } else {
                                    Vec::new()
                                };
                            let loop_ = build!(
                                active,
                                &turn_cancel,
                                ports,
                                observation,
                                probe_evidence,
                                session_hooks,
                                session_journal,
                                skill_owner
                            );
                            let loop_ = match resumed_turn.as_ref() {
                                Some(RuntimeResume::Foreign { policy, .. }) => loop_
                                    .with_hook_policy(Arc::clone(policy))
                                    .map_err(|message| RunTermination::Failed { message })?,
                                Some(RuntimeResume::Model(_)) | None => loop_,
                            };
                            let mut loop_ = loop_.with_interactive_control(&mut control);
                            let driven = match resumed_turn {
                                Some(RuntimeResume::Model(turn)) => {
                                    let (termination, result) = turns
                                        .drive_resumed(&mut loop_, turn, &turn_cancel, diagnostics)
                                        .await;
                                    Some((termination, result))
                                }
                                Some(RuntimeResume::Foreign {
                                    question, answer, ..
                                }) => {
                                    let (termination, result) = turns
                                        .drive_answered(
                                            &mut loop_,
                                            question,
                                            answer,
                                            &turn_cancel,
                                            diagnostics,
                                        )
                                        .await;
                                    Some((termination, result))
                                }
                                None => match asked {
                                    Some(InteractiveTurnInput::Prompt(asked)) => {
                                        let (termination, result) = turns
                                            .drive_with_images(
                                                &mut loop_,
                                                &asked,
                                                images,
                                                &turn_cancel,
                                                diagnostics,
                                            )
                                            .await;
                                        Some((termination, result))
                                    }
                                    Some(InteractiveTurnInput::Skill(operation)) => {
                                        let (termination, result) = turns
                                            .drive_admitted_skill(
                                                &mut loop_,
                                                operation,
                                                &turn_cancel,
                                                diagnostics,
                                            )
                                            .await;
                                        Some((termination, result))
                                    }
                                    None => match loop_.run_queued_turn().await {
                                        Ok(Some(outcome)) => {
                                            let termination = loop_
                                                .state()
                                                .pending_question()
                                                .map(|pending| RunTermination::QuestionRequired {
                                                    question: Box::new(
                                                        QuestionReport::from_request(
                                                            pending.item_id(),
                                                            pending.target(),
                                                            pending.request(),
                                                        ),
                                                    ),
                                                })
                                                .or_else(|| {
                                                    RunTermination::after_turn(
                                                        outcome.exit_reason,
                                                        outcome.denied,
                                                    )
                                                })
                                                .unwrap_or(RunTermination::Done);
                                            Some((
                                                termination,
                                                Value::String(last_assistant_message(
                                                    &loop_.state(),
                                                )),
                                            ))
                                        }
                                        Ok(None) => None,
                                        Err(abandoned) => Some((
                                            RunTermination::Failed {
                                                message: format!(
                                                    "the session log could not be written: {}",
                                                    abandoned.cause
                                                ),
                                            },
                                            Value::String(last_assistant_message(&loop_.state())),
                                        )),
                                    },
                                },
                            };
                            drop(loop_);
                            state = session_hooks.state.state();
                            if let Some(status) = status.as_ref()
                                && status.human
                            {
                                match policy_selection.snapshot() {
                                    Ok(policy) => status.seat.set_status_mode(policy.mode),
                                    Err(error) => diagnostics.note(&format!(
                                        "the active policy mode could not be refreshed: {error}"
                                    )),
                                }
                                match selection.snapshot() {
                        Ok(selected) => status.seat.refresh_status(&state, selected),
                        Err(error) => diagnostics.note(&format!(
                            "the native status selection could not be refreshed: {error}"
                        )),
                    }
                            }
                            seat.refresh_queue(control.status());
                            let Some((termination, result)) = driven else {
                                continue;
                            };
                            between();

                            let queue_abandoned =
                                matches!(&termination, RunTermination::Failed { .. });
                            last = Ended {
                                termination,
                                session: active.id(),
                                state: state.clone(),
                                result,
                                mcp_errors: mcp_errors.clone(),
                            };

                            // The person's own cancellations end a turn; the process's end
                            // the process. A broken stdout or a SIGTERM is not a turn to
                            // return to a prompt from.
                            if cancel.is_cancelled() {
                                break 'epoch EpochTransition::Exit;
                            }
                            if queue_abandoned {
                                break 'epoch EpochTransition::Exit;
                            }
                        }
                    };

                    drop(model_switch_owner);
                    let session_end = match skill_owner.drain().await {
                        Ok(()) => {
                            let runtime = match recovered_epoch_policy {
                                Some(policy) => session_hooks.bind_policy(policy),
                                None => session_hooks.bind_current(policy_source.as_ref()),
                            };
                            match runtime {
                                Ok(runtime) => {
                                    dispatch_session_hook(
                                        &runtime,
                                        active.id(),
                                        session_hooks.turn,
                                        HookLifecycle::SessionEnd,
                                    )
                                    .await
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(RunTermination::Failed {
                            message: error.to_string(),
                        }),
                    };
                    drop(skill_owner);
                    drop(session_hooks);
                    drop(session_journal);
                    let source_state = leased_session_journal
                        .try_into_state()
                        .map_err(hook_journal_termination)?;
                    drop(session_hook_ports);
                    (transition, source_state, session_end)
                };

                match transition {
                    EpochTransition::Exit => {
                        active.replace_state(source_state.clone());
                        last.state = source_state;
                        if let Err(termination) = session_end {
                            last.termination = termination;
                        }
                        break 'sessions Ok(last);
                    }
                    EpochTransition::New { target } => {
                        active.replace_state(source_state);
                        if let Err(error) = session_end {
                            diagnostics.note(&format!(
                                "the previous session-end hook failed: {}",
                                error.exit_reason()
                            ));
                        }
                        active = target;
                        epoch_start = EpochStart::New;
                    }
                    EpochTransition::Fork {
                        target,
                        publication,
                        diagnostic,
                        projected,
                    } => {
                        active.replace_state(source_state);
                        if !active.apply_publication(target, &publication) {
                            return Err(RunTermination::Failed {
                                message: format!(
                                    "the published fork target could not be activated: {}",
                                    diagnostic.as_deref().unwrap_or("publication was refused")
                                ),
                            });
                        }
                        if let Err(error) = session_end {
                            diagnostics.note(&format!(
                                "the fork source session-end hook failed: {}",
                                error.exit_reason()
                            ));
                        }
                        epoch_start = EpochStart::Fork {
                            diagnostic,
                            projected: projected.map(Box::new),
                        };
                    }
                    EpochTransition::Compact {
                        target,
                        publication,
                        diagnostic,
                        runtime,
                        request,
                        parent,
                    } => {
                        active.replace_state(source_state);
                        if !active.apply_publication(target, &publication) {
                            return Err(RunTermination::Failed {
                                message: "the published compaction target could not be activated"
                                    .to_owned(),
                            });
                        }
                        if let Err(error) = session_end {
                            diagnostics.note(&format!(
                                "the compacted source session-end hook failed: {}",
                                error.exit_reason()
                            ));
                        }
                        if let Some((target_system, target_registry, prepared_skills)) = runtime {
                            if let Some(prepared) = prepared_skills {
                                let published = prepared.published();
                                let Some(catalogue) = skills.as_ref() else {
                                    return Err(RunTermination::Failed {
                                        message: "the prepared skill catalogue lost its publication owner"
                                            .to_owned(),
                                    });
                                };
                                let capture = catalogue.commit_rescan(prepared).map_err(|error| {
                                    RunTermination::Failed {
                                        message: format!(
                                            "the prepared skill catalogue could not be published after compaction activation: {error}"
                                        ),
                                    }
                                })?;
                                if published {
                                    diagnostics.note(&format!(
                                        "skills: published generation {}",
                                        capture.snapshot_digest()
                                    ));
                                } else {
                                    diagnostics.note(&format!(
                                        "skills: retained last-good generation {}",
                                        capture.snapshot_digest()
                                    ));
                                    diagnostics.note(&capture.render_diagnostics());
                                }
                            }
                            registry = target_registry;
                            config.system = target_system;
                        }
                        epoch_start = EpochStart::Compact {
                            diagnostic,
                            request,
                            parent,
                        };
                    }
                }
            }
        }
    }
}

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
mod tests {
    use super::*;
    use crate::render::caps::{Depth, OscProfile};
    use ego_core::event::domain::{DomainEvent, Envelope};
    use ego_core::event::hook::{HookDecision, HookOutcome, HookSource};
    use ego_core::event::id::Seq;
    use ego_core::event::skill::{SkillInvocationId, SkillSourceAuthority};
    use ego_core::event::time::{Clock, Timestamp};
    use ego_core::hook::{
        HookConfigEntry, HookFuture, HookHandlerConfig, HookInvocationPort, HookInvocationRequest,
    };
    use ego_core::question::{
        QuestionAnswer, QuestionMode, QuestionOption, QuestionRequest, QuestionTarget,
    };
    use ego_core::run::RunOrigin;
    use ego_core::store::evidence::{EvidenceId, EvidenceRef};
    use ego_core::store::session_store::SessionMeta;
    use ego_core::store::session_store::{SessionStore, StoreError};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    struct UnusedStore;

    impl ego_core::config::UserConfigStore for UnusedStore {
        fn snapshot(
            &self,
            _target: ego_core::config::UserConfigTarget,
        ) -> Result<ego_core::config::UserConfigSnapshot, ego_core::config::UserConfigError>
        {
            panic!("remember preview must not read the store")
        }

        fn rule_snapshot(
            &self,
            _target: ego_core::config::UserConfigTarget,
        ) -> Result<ego_core::config::RuleSnapshot, ego_core::config::UserConfigError> {
            panic!("remember preview must not read the store")
        }

        fn mutate(
            &self,
            _mutation: ego_core::config::UserConfigMutation,
        ) -> Result<ego_core::config::UserConfigMutationResult, ego_core::config::UserConfigError>
        {
            panic!("remember preview must not mutate the store")
        }
    }

    #[derive(Default)]
    struct HookCaptureStore;

    impl SessionStore for HookCaptureStore {
        fn append(&self, _id: &SessionId, _event: &DomainEvent) -> Result<(), StoreError> {
            Ok(())
        }

        fn read(
            &self,
            _id: &SessionId,
        ) -> Result<futures::stream::BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError>
        {
            Ok(Box::pin(futures::stream::empty()))
        }

        fn compare_and_append_run(
            &self,
            _session: &SessionId,
            _expected_revision: Option<ego_core::run::RunRevision>,
            _transaction: ego_core::run::RunTransaction,
        ) -> Result<ego_core::run::RunAppend, StoreError> {
            Err(StoreError::NotFound)
        }

        fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
            Ok(Vec::new())
        }
    }

    struct HookCaptureClock;

    impl Clock for HookCaptureClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(1_000)
        }
    }

    #[derive(Default)]
    struct HookCaptureInvoker;

    impl HookInvocationPort for HookCaptureInvoker {
        fn invoke<'a>(&'a self, _request: HookInvocationRequest) -> HookFuture<'a> {
            Box::pin(std::future::ready(Ok(HookOutcome::new(
                HookDecision::Continue,
                None,
            ))))
        }
    }

    fn pending_resume_state(
        target: QuestionTarget,
    ) -> (SessionState, ResumeAnswer, TurnId, EventId) {
        let question = ItemId::new();
        let turn = TurnId::new();
        let request_event = EventId::new();
        let request = QuestionRequest::new(
            "Continue?",
            QuestionMode::Single,
            vec![QuestionOption::new("yes", "Yes")],
            false,
        );
        let mut state = SessionState::default();
        state.apply(&DomainEvent {
            env: Envelope {
                seq: Seq::default(),
                id: request_event,
                turn,
                item: Some(question),
                parent: None,
                ts: Timestamp::from_millis(1_000),
                producer: Producer::Main,
            },
            kind: EventKind::QuestionRequested { request, target },
        });
        assert_eq!(
            state.pending_question().map(|pending| pending.item_id()),
            Some(question)
        );
        (
            state,
            ResumeAnswer {
                id: question,
                json: r#""yes""#.to_owned(),
            },
            turn,
            request_event,
        )
    }

    #[derive(Default, Clone)]
    struct LaunchBarrierStore {
        events: Arc<Mutex<Vec<DomainEvent>>>,
        transactions: Arc<Mutex<Vec<ego_core::run::RunTransaction>>>,
    }

    impl SessionStore for LaunchBarrierStore {
        fn append(&self, _id: &SessionId, event: &DomainEvent) -> Result<(), StoreError> {
            self.events.lock().unwrap().push(event.clone());
            Ok(())
        }

        fn read(
            &self,
            _id: &SessionId,
        ) -> Result<futures::stream::BoxStream<'static, Result<SessionEntry, StoreError>>, StoreError>
        {
            let events = self.events.lock().unwrap().clone();
            let transactions = self.transactions.lock().unwrap().clone();
            Ok(Box::pin(futures::stream::iter(
                events
                    .into_iter()
                    .map(|event| Ok(SessionEntry::Event(event)))
                    .chain(
                        transactions
                            .into_iter()
                            .map(|transaction| Ok(SessionEntry::RunTransaction(transaction))),
                    ),
            )))
        }

        fn compare_and_append_run(
            &self,
            _session: &SessionId,
            _expected_revision: Option<ego_core::run::RunRevision>,
            transaction: ego_core::run::RunTransaction,
        ) -> Result<ego_core::run::RunAppend, StoreError> {
            let revision = transaction.revision();
            self.transactions.lock().unwrap().push(transaction);
            Ok(ego_core::run::RunAppend::Committed { revision })
        }

        fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
            Ok(Vec::new())
        }
    }

    #[derive(Default)]
    struct LaunchBarrierClock;

    impl Clock for LaunchBarrierClock {
        fn now(&self) -> Timestamp {
            Timestamp::from_millis(1_000)
        }
    }

    #[derive(Default)]
    struct LaunchBarrierIds;

    impl ego_core::run::IdSource for LaunchBarrierIds {
        fn next_uuid(&self) -> uuid::Uuid {
            uuid::Uuid::now_v7()
        }
    }

    struct LaunchBarrierSink;

    impl ego_core::run::EventSinkFactory for LaunchBarrierSink {
        fn create(&self, _capabilities: &ModelCapabilities) -> Arc<dyn EventSink> {
            Arc::new(NullSink)
        }
    }

    struct LaunchBarrierExecutor {
        crossed: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl LaunchBarrierExecutor {
        fn arm(&self) -> tokio::sync::oneshot::Receiver<()> {
            let (sender, receiver) = tokio::sync::oneshot::channel();
            *self.crossed.lock().unwrap() = Some(sender);
            receiver
        }
    }

    impl RunExecutor for LaunchBarrierExecutor {
        type Prepared = ();

        fn prepare<'a>(
            &'a self,
            _request: &'a RunPreparation,
        ) -> futures::future::BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>>
        {
            Box::pin(std::future::ready(Ok(RunPrepared::new(
                ModelRef("launch-test-model".to_owned()),
                (),
            ))))
        }

        fn admit_run_context<'a>(
            &'a self,
            _prepared: &'a Self::Prepared,
        ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
            Box::pin(std::future::ready(Ok(EvidenceWrite::Published)))
        }

        fn attach<'a>(
            &'a self,
            request: &'a RunAttachment,
        ) -> futures::future::BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
            Box::pin(std::future::ready(Ok(RunHandle::new(request.run_id, ()))))
        }

        fn recover<'a>(
            &'a self,
            _run: &'a RunId,
        ) -> futures::future::BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
            Box::pin(std::future::ready(Ok(RecoveryEvidence::NotRecoverable)))
        }

        fn observe<'a>(
            &'a self,
            _handle: &'a RunHandle,
            _bindings: RunExecutionBindings,
        ) -> futures::future::BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
            Box::pin(std::future::ready(Ok(RunSettlement::Completed)))
        }

        fn submit<'a>(
            &'a self,
            _handle: &'a RunHandle,
            _prompt: &'a RunPrompt,
            _prepared: Self::Prepared,
            _bindings: RunExecutionBindings,
        ) -> futures::future::BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
            if let Some(sender) = self.crossed.lock().unwrap().take() {
                sender
                    .send(())
                    .expect("launch barrier receiver remains alive");
            }
            Box::pin(std::future::ready(Ok(RunSettlement::Completed)))
        }

        /// A no-op for the same reason `interrupt` is one: this double never
        /// runs a turn, so there is nothing here that a hold could hold back.
        fn pause<'a>(
            &'a self,
            _handle: &'a RunHandle,
            _request: RequestId,
        ) -> futures::future::BoxFuture<'a, Result<(), RunExecutorError>> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn interrupt<'a>(
            &'a self,
            _handle: &'a RunHandle,
        ) -> futures::future::BoxFuture<'a, Result<(), RunExecutorError>> {
            Box::pin(std::future::ready(Ok(())))
        }
    }

    /// The launch-only fixture host: it answers what a launch needs and
    /// refuses what a launch does not reach.
    ///
    /// One fake for one port. Six fakes existed because six ports did, and
    /// every one of them was constructed in the same breath as the others —
    /// which is the evidence that none of them was a boundary.
    struct LaunchBarrierHost(&'static LaunchBarrierStore);

    impl ChildHost for LaunchBarrierHost {
        fn read_journal(
            &self,
            _invocation: SubagentInvocationId,
        ) -> Result<Vec<DomainEvent>, ChildJournalError> {
            Ok(self.0.events.lock().unwrap().clone())
        }

        fn read_rendered_task(
            &self,
            _reference: &BlobRef,
        ) -> Result<Vec<u8>, ChildResultReaderError> {
            Ok(b"background launch task".to_vec())
        }

        fn read_final_assistant(
            &self,
            run: RunId,
        ) -> Result<ChildResultEvidence, ChildResultReaderError> {
            Err(ChildResultReaderError::Missing { run })
        }

        fn copy_to_parent(
            &self,
            _child: RunId,
            _evidence: &ChildResultEvidence,
        ) -> Result<BlobRef, ChildResultReaderError> {
            Err(ChildResultReaderError::Read {
                diagnostic: "collection is outside this launch-only fixture".to_owned(),
            })
        }

        fn resolve_profile(&self, _name: &str) -> Result<(), String> {
            Ok(())
        }

        fn admit_selections(
            &self,
            request: &ChildRequest,
            bindings: &RunExecutionBindings,
        ) -> Result<(RunSelections, RunExecutionBindings), String> {
            Ok((request.selections().clone(), bindings.clone()))
        }
    }

    fn launch_barrier_parent_hooks(
        store: &'static LaunchBarrierStore,
        session: SessionId,
    ) -> SessionHookRuntime<'static> {
        let journal = SharedJournal::new(Journal::new(
            session,
            store,
            &LaunchBarrierClock,
            &NullSink,
            Producer::Main,
            {
                let mut state = SessionState::default();
                for event in store.events.lock().unwrap().iter() {
                    state.apply(event);
                }
                state
            },
        ));
        let catalogue = HookCatalogue::admit_typed(&[], &[], Trust::Trusted)
            .expect("empty launch barrier hook catalogue");
        SessionHookState::new(
            HookLifecycleDispatcher::new(HookRunner::new(), catalogue),
            journal,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(30),
        )
        .bind(Arc::new(HookCaptureInvoker))
    }

    #[tokio::test]
    async fn cli_background_launch_crosses_execution_barrier_without_collection_future() {
        let store: &'static LaunchBarrierStore = Box::leak(Box::new(LaunchBarrierStore::default()));
        let executor = Arc::new(LaunchBarrierExecutor {
            crossed: Mutex::new(None),
        });
        let crossed = executor.arm();
        let parent_run = RunId::new(SessionId::new(), uuid::Uuid::now_v7());
        let parent_session = parent_run.session_id();
        let gate = SharedBudgetGate::new(ego_core::budget::BudgetGate::unlimited(
            CurrencyCode::new("USD").expect("launch barrier currency"),
        ));
        let bindings = RunExecutionBindings::new(Arc::new(LaunchBarrierSink), None)
            .with_budget_gate(gate.clone());
        let ports = ChildServicePorts::new(parent_run, Arc::new(LaunchBarrierHost(store)));
        let controller = Arc::new(RunController::new(
            Arc::new(store.clone()),
            Arc::clone(&executor),
            Arc::new(LaunchBarrierClock),
            Arc::new(LaunchBarrierIds),
            RunRetention::new(NonZeroUsize::new(8).expect("retention")).expect("retention"),
        ));
        let service = ChildRunService::new(
            controller,
            gate,
            RunRoot::new("/workspace").expect("launch barrier root"),
            bindings,
            ports,
        );
        let invocation = SubagentInvocationId::new();
        let parent_turn = TurnId::new();
        let parent_item = ItemId::new();
        let parent_event = EventId::new();
        store
            .append(
                &parent_session,
                &DomainEvent {
                    env: Envelope {
                        seq: Seq::default(),
                        id: parent_event,
                        turn: parent_turn,
                        item: Some(parent_item),
                        parent: None,
                        ts: Timestamp::from_millis(1_000),
                        producer: Producer::Main,
                    },
                    kind: EventKind::ToolCallStarted {
                        name: "skill".to_owned(),
                        args: "{}".to_owned(),
                        call_id: Some("launch-barrier".to_owned()),
                    },
                },
            )
            .expect("parent event is durable before child admission");
        let hooks = launch_barrier_parent_hooks(store, parent_session);
        let task = b"background launch task";
        service
            .start(
                ChildRequest::new(
                    invocation,
                    ChildRequestOrigin::new(parent_run, parent_turn, parent_item, parent_event),
                    BlobRef::new(
                        BlobId::parse(ego_core::digest::sha256_hex(task)).expect("task blob id"),
                        task.len() as u64,
                    ),
                    RunSelections::empty(),
                    ChildWait::Background,
                )
                .with_requested_tools(["read"]),
                &hooks,
            )
            .await
            .expect("background child admission");
        let owner = CliSkillInvocationOwner::new(service, hooks, CancelToken::new());
        let execution = owner
            .launch(invocation)
            .expect("launch does not fail")
            .expect("admitted background execution is transferred");
        let completed = tokio::time::timeout(Duration::from_secs(1), execution)
            .await
            .expect("background execution does not stall")
            .expect("background execution task does not panic");
        completed.expect("background execution settles");
        tokio::time::timeout(Duration::from_secs(1), crossed)
            .await
            .expect("child execution crosses its barrier before collection exists")
            .expect("launch barrier sender remains alive");
    }

    #[test]
    fn workspace_accepts_a_directory_but_rejects_an_existing_regular_file() {
        let owner = tempdir().expect("workspace owner");
        let directory = owner.path().join("workspace");
        std::fs::create_dir(&directory).expect("workspace directory");
        let regular_file = owner.path().join("workspace-file");
        std::fs::write(&regular_file, b"not a workspace").expect("workspace file");

        let directory_arg = directory.clone();
        assert_eq!(
            workspace(Some(&directory_arg)).expect("directory is a workspace"),
            std::fs::canonicalize(directory).expect("canonical workspace")
        );

        let file_arg = regular_file.clone();
        assert!(
            workspace(Some(&file_arg)).is_err(),
            "an existing regular file must be rejected as a workspace root: {}",
            regular_file.display()
        );
    }

    /// A refusal an operator can act on names four things.
    ///
    /// The mode that asked, the guarantee that is missing in words, the
    /// backend that failed to demonstrate it, and the way out. The bitflag
    /// `Debug` text named none of them.
    #[test]
    fn a_capability_refusal_names_the_mode_the_guarantee_the_backend_and_the_remedies() {
        let owner = tempdir().expect("refusal fixture owner");
        let workspace = CanonPath::new(owner.path().to_path_buf());
        let confinement = RunConfinement::new(Confinement::Workspace, false, workspace)
            .expect("workspace confinement");
        let missing = ExecCaps::READ_SCOPED
            .union(ExecCaps::WRITE_SCOPED)
            .check(ExecCaps::READ_SCOPED)
            .expect_err("a backend without scoped writes cannot serve workspace confinement")
            .with_probe_evidence(Some(EvidenceRef {
                id: EvidenceId::parse("a".repeat(64)).expect("probe transcript identity"),
                bytes: 128,
            }));

        let termination = process_error_termination(
            ProcessError::MissingCapabilities(missing),
            &confinement,
            &CancelToken::new(),
        );
        let RunTermination::UsageError { message } = termination else {
            panic!("a backend that cannot demonstrate the requested sandbox is a usage refusal");
        };

        assert!(
            message.contains("--sandbox workspace"),
            "the refusal names the mode that asked: {message}"
        );
        assert!(
            message.contains("write only inside the workspace and ego's private scratch directory"),
            "the refusal names the missing guarantee in words: {message}"
        );
        assert!(
            message.contains(if cfg!(target_os = "macos") {
                "macOS Seatbelt"
            } else if cfg!(target_os = "linux") {
                "Linux Landlock"
            } else {
                "no confinement backend"
            }),
            "the refusal names the platform backend that failed: {message}"
        );
        assert!(
            message.contains("`--sandbox ro`") && message.contains("`--sandbox off`"),
            "the refusal offers the remedy of asking for less: {message}"
        );
        assert!(
            message.contains(&"a".repeat(64)),
            "the refusal cites the measurement that diagnosed the cause: {message}"
        );
        assert!(
            !message.contains("ExecCaps("),
            "the refusal must not fall back on the bitflag debug text: {message}"
        );
    }

    #[tokio::test]
    async fn protocol_ci_profile_is_refused_before_session_or_provider_effects() {
        let owner = tempdir().expect("protocol profile fixture owner");
        let workspace = owner.path().join("workspace");
        let session_root = owner.path().join("sessions");
        std::fs::create_dir(&workspace).expect("protocol fixture workspace");
        let store = Arc::new(JsonlStore::new(
            session_root.clone(),
            workspace.clone(),
            Box::new(OsIo),
        ));
        let invocation = RunInvocationId::new();
        let request = RunPreparation {
            run_id: RunId::new(SessionId::new(), invocation.uuid()),
            invocation,
            root: ego_core::run::RunRoot::new(workspace).expect("protocol fixture root"),
            selections: ego_core::run::RunSelections::new(
                None,
                None,
                None,
                None,
                Some("ci".to_owned()),
            ),
            continuation: false,
            origin: RunOrigin::TopLevel,
        };

        let admission = Arc::new(EvidenceAdmissionService::new());
        let child_executor = Arc::new(
            CliRunExecutor::for_acp(None, Arc::clone(&store), Arc::clone(&admission))
                .for_children(),
        );
        let result = CliPrepared::prepare(
            protocol_args(None),
            store,
            None,
            &request,
            None,
            None,
            PreparedExecutionRuntime {
                admission,
                child_executor,
                child: false,
                child_authority: None,
            },
        )
        .await;
        let message = match result {
            Err(RunTermination::UsageError { message }) => message,
            Err(_) => panic!("protocol ci selection must be a usage refusal"),
            Ok(_) => panic!("protocol ci selection must be refused"),
        };
        assert_eq!(
            message,
            "profile ci available only through ego run or ego resume"
        );
        assert!(
            !session_root.exists(),
            "protocol profile refusal must precede session creation"
        );
    }

    #[test]
    fn durable_compact_is_enabled_when_idle_before_confirmation_resolution_generation_or_publication()
     {
        let source = SessionId::new();
        let root: &'static tempfile::TempDir =
            Box::leak(Box::new(tempdir().expect("durable evidence owner")));
        let store = Box::leak(Box::new(crate::session::jsonl(
            root.path().to_path_buf(),
            PathBuf::from("/workspace"),
        )));
        let opened =
            crate::session::open(store, &source, &SystemClock).expect("durable session writer");
        let active = crate::session::ActiveSession::durable(opened, SessionState::default(), 1024)
            .expect("durable active session");

        assert_eq!(
            super::compaction_availability(false),
            repl::actions::Availability::Enabled
        );
        assert_eq!(active.id(), source);
        assert_eq!(active.state(), &SessionState::default());
    }

    #[derive(Debug)]
    struct CompactionDecisionSeat {
        confirm: bool,
        activate: bool,
    }

    impl Reads for CompactionDecisionSeat {
        fn read(&self) -> repl::Line {
            repl::Line::Ended
        }
    }

    impl InteractiveSeat for CompactionDecisionSeat {
        fn has_human(&self) -> bool {
            true
        }

        fn read_startup_model(
            &self,
            _picker: Arc<Mutex<ModelPicker>>,
            _notice: Option<String>,
        ) -> crate::model_picker::PickerOutcome {
            panic!("compaction fixture must not ask for startup model selection");
        }

        fn trust(&self, _workspace: &Path) -> Trust {
            Trust::Trusted
        }

        fn permission(&self, _request: &PermissionRequest) -> PermissionDecision {
            PermissionDecision::Deny
        }

        fn confirm_compaction(&self, _source: SessionId) -> bool {
            self.confirm
        }

        fn activate_session(&self, _target: SessionId) -> bool {
            self.activate
        }
    }

    #[test]
    fn compaction_decline_is_a_control_decision_and_keeps_the_source_selected() {
        let source = SessionId::new();
        let seat = CompactionDecisionSeat {
            confirm: false,
            activate: true,
        };

        assert!(!seat.confirm_compaction(source));
    }

    #[test]
    fn activation_failure_is_reported_after_publication_without_a_revert_request() {
        let target = SessionId::new();
        let seat = CompactionDecisionSeat {
            confirm: true,
            activate: false,
        };

        assert!(seat.confirm_compaction(SessionId::new()));
        assert!(!seat.activate_session(target));
    }

    #[derive(Debug)]
    struct TrustingSeat {
        asked: AtomicUsize,
    }

    impl Reads for TrustingSeat {
        fn read(&self) -> repl::Line {
            repl::Line::Ended
        }
    }

    impl InteractiveSeat for TrustingSeat {
        fn has_human(&self) -> bool {
            true
        }

        fn read_startup_model(
            &self,
            _picker: Arc<Mutex<ModelPicker>>,
            _notice: Option<String>,
        ) -> crate::model_picker::PickerOutcome {
            panic!("trust fixture must not ask for startup model selection");
        }

        fn trust(&self, _workspace: &Path) -> Trust {
            self.asked.fetch_add(1, Ordering::SeqCst);
            Trust::Trusted
        }

        fn permission(&self, _request: &PermissionRequest) -> PermissionDecision {
            PermissionDecision::Deny
        }
    }

    fn interactive_args(extra: &[&str]) -> Args {
        let argv = std::iter::once("ego".to_string())
            .chain(extra.iter().map(|arg| arg.to_string()))
            .collect();
        let crate::cli::Invocation::Interactive(args) = crate::cli::parse(argv).unwrap() else {
            panic!("the test invocation is interactive");
        };
        *args
    }

    #[test]
    fn only_an_interactive_non_bare_run_can_ask_for_trust() {
        let seat = Arc::new(TrustingSeat {
            asked: AtomicUsize::new(0),
        });
        let dyn_seat: Arc<dyn InteractiveSeat> = seat.clone();
        let between = || {};
        let interactive = Driver::Interactive {
            seat: dyn_seat,
            between: &between,
        };
        // A workspace with something to trust and a home of its own, so the
        // routing under test is the only thing that decides whether the seat
        // is consulted. What the *store* then does with the answer is
        // `trust.rs`'s contract, not this one's.
        let workspace = tempfile::tempdir().expect("a temp dir");
        let home = tempfile::tempdir().expect("a temp dir");
        std::fs::write(workspace.path().join("AGENTS.md"), "# here\n")
            .expect("a temp dir is writable");
        let root = workspace.path();
        let home = Some(home.path());

        assert_eq!(
            run_trust(&interactive_args(&[]), &interactive, root, home, &Stderr),
            Trust::Trusted
        );
        assert_eq!(seat.asked.load(Ordering::SeqCst), 1);

        assert_eq!(
            run_trust(
                &interactive_args(&["--bare"]),
                &interactive,
                root,
                home,
                &Stderr
            ),
            Trust::Untrusted
        );
        assert_eq!(
            seat.asked.load(Ordering::SeqCst),
            1,
            "--bare must skip the seat"
        );

        assert_eq!(
            run_trust(&interactive_args(&[]), &Driver::Once, root, home, &Stderr),
            Trust::Untrusted,
            "headless has no seat to consult"
        );
        assert_eq!(seat.asked.load(Ordering::SeqCst), 1);
    }

    fn capabilities(osc: OscProfile) -> Capabilities {
        Capabilities {
            depth: Depth::TrueColor,
            width: 80,
            hyperlinks: true,
            progress: true,
            osc,
        }
    }

    fn announce(osc: OscProfile, quiet: bool) -> (Stdout, Capabilities, bool) {
        (Stdout::new(CancelToken::new()), capabilities(osc), quiet)
    }

    /// The reason any of this exists: TUICommander segments a pane into command
    /// blocks from 133 A/C/D and otherwise falls back to pattern-matching
    /// tool-call headers (`tuicommander/docs/FEATURES.md:187`). C opens the
    /// block, D closes it carrying the exit status, and nothing may come after
    /// D — anything that did would belong to the next command.
    #[test]
    fn the_block_opens_with_c_and_closes_with_d_carrying_the_status() {
        let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
        let announce = Announce::new(&out, capabilities, quiet);

        assert!(announce.preamble(Some("hello")).ends_with("\x1b]133;C\x07"));

        let failed = announce.postamble(&RunTermination::TaskFailed, Duration::ZERO);
        assert!(failed.ends_with("\x1b]133;D;1\x07"), "{failed:?}");
        let done = announce.postamble(&RunTermination::Done, Duration::ZERO);
        assert!(done.ends_with("\x1b]133;D;0\x07"), "{done:?}");
    }

    #[test]
    fn a_title_is_branded_once_and_closing_keeps_only_the_fixed_prefix() {
        let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
        let announce = Announce::new(&out, capabilities, quiet);

        assert_eq!(
            announce.preamble(Some("explain the diff")),
            "\x1b]0;ɛ । explain the diff\x07\x1b]9;4;3;0\x07\x1b]133;C\x07"
        );
        assert_eq!(
            announce.preamble(None),
            "\x1b]0;ɛ । εgo\x07\x1b]9;4;3;0\x07\x1b]133;C\x07"
        );
        assert_eq!(
            announce.postamble(&RunTermination::Done, Duration::ZERO),
            "\x1b]9;4;0;0\x07\x1b]0;ɛ । \x07\x1b]133;D;0\x07"
        );
    }

    /// A progress indicator left set outlives the process that set it: the tab
    /// stays marked until the terminal is restarted. So it is cleared on the
    /// failure path too, and the failure is reported by 133 D instead.
    #[test]
    fn the_indicator_is_always_taken_down_however_the_run_ended() {
        let (out, capabilities, quiet) = announce(OscProfile::Standard, false);
        let announce = Announce::new(&out, capabilities, quiet);

        assert!(
            announce.preamble(None).contains("\x1b]9;4;3;0\x07"),
            "a run of unknown length is indeterminate"
        );
        for termination in [
            RunTermination::Done,
            RunTermination::TaskFailed,
            RunTermination::Signalled(Signal::Int),
        ] {
            let closing = announce.postamble(&termination, Duration::ZERO);
            let state = if termination.exit_code() == 0 { 0 } else { 2 };
            assert!(
                closing.contains(&format!("\x1b]9;4;{state};0\x07")),
                "{termination:?} left the indicator: {closing:?}"
            );
        }
    }

    /// §4.5, applied to the out-of-band channel. A JSON document with an OSC in
    /// it is not JSON, and the gate is what prevents that rather than a rule
    /// each call site remembers.
    #[test]
    fn a_machine_readable_run_announces_nothing_at_all() {
        let out = Stdout::new(CancelToken::new());
        let announce = Announce::new(&out, Capabilities::plain(), false);
        assert_eq!(announce.preamble(Some("hello")), "");
        assert_eq!(
            announce.postamble(&RunTermination::Done, Duration::from_secs(3_600)),
            ""
        );
    }

    /// A notification is an interruption. It is worth it for a run the operator
    /// stopped watching, and it is noise for one they saw finish — so the
    /// duration decides, and `-q` overrules the duration.
    #[test]
    fn only_a_long_run_is_worth_interrupting_someone_over() {
        let (out, capabilities, _) = announce(OscProfile::Standard, false);
        let loud = Announce::new(&out, capabilities, false);

        // `\x1b]9;εgo`, not `\x1b]9;` — OSC 9;4 is the progress indicator and
        // shares the prefix.
        assert!(
            !loud
                .postamble(&RunTermination::Done, NOTIFY_AFTER - Duration::from_secs(1))
                .contains("\x1b]9;εgo"),
            "a run somebody watched finish"
        );
        let long = loud.postamble(&RunTermination::Done, Duration::from_secs(252));
        assert!(
            long.contains("\x1b]9;εgo: done after 4m 12s\x07"),
            "{long:?}"
        );

        let quiet = Announce::new(&out, capabilities, true);
        assert!(
            !quiet
                .postamble(&RunTermination::Done, Duration::from_secs(252))
                .contains("\x1b]9;εgo"),
            "-q suppresses it at any duration"
        );
    }

    /// The private protocol reaches TUIC and nowhere else, and it is the
    /// capability gate that decides — not a call site that checks a variable.
    #[test]
    fn only_tuic_is_told_what_ego_is_doing() {
        let (out, standard, _) = announce(OscProfile::Standard, false);
        let elsewhere = Announce::new(&out, standard, false);
        assert!(!elsewhere.preamble(None).contains("7770"));

        let inside = Announce::new(&out, capabilities(OscProfile::Tuic), false);
        assert!(inside.preamble(None).contains("\x1b]7770;state=busy\x07"));
        assert!(
            inside
                .postamble(&RunTermination::Done, Duration::ZERO)
                .contains("\x1b]7770;state=idle\x07")
        );
    }

    #[test]
    fn the_title_says_what_was_asked_and_never_says_a_dash() {
        assert_eq!(title(Some("explain the diff")), "explain the diff");
        // `-` is the spelling for "the prompt is on stdin".
        assert_eq!(title(Some("-")), "εgo");
        assert_eq!(title(None), "εgo");
        assert_eq!(title(Some("   ")), "εgo");
        // A prompt is often a paragraph; a tab is one line.
        assert_eq!(title(Some("first line\nsecond line")), "first line");
    }

    /// Cut in cells, like everything else that reaches a terminal. A title cut
    /// by bytes puts a replacement character in the tab.
    #[test]
    fn a_long_title_is_cut_where_a_grapheme_ends() {
        let flag = "\u{1f1ee}\u{1f1f9}";
        let title = title(Some(&flag.repeat(40)));
        assert!(!title.contains('\u{fffd}'));
        assert_eq!(crate::render::line::cells(&title), 48);
    }

    #[test]
    fn a_duration_is_written_for_a_person() {
        assert_eq!(duration(Duration::from_secs(9)), "9s");
        assert_eq!(duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(duration(Duration::from_secs(252)), "4m 12s");
    }

    #[test]
    fn failed_skill_tool_preflight_keeps_owner_view_and_model_registry_unchanged() {
        let owner = tempdir().expect("skill preflight owner");
        let system = owner.path().join("system/skills");
        let user = owner.path().join("user/skills");
        let project = owner.path().join("project/skills");
        for root in [&system, &user, &project] {
            std::fs::create_dir_all(root).expect("skill root");
        }
        let explain = user.join("explain");
        std::fs::create_dir_all(&explain).expect("skill directory");
        std::fs::write(
            explain.join("SKILL.md"),
            "---\nname: explain\ndescription: Explain evidence.\n---\nExplain.\n",
        )
        .expect("skill file");
        let discovery = crate::skill::SkillDiscovery::new(system, user.clone(), project, false);
        let catalogue =
            crate::skill::SkillCatalogue::discover(discovery, &["help", "skills", "hooks", "quit"])
                .expect("skill catalogue");
        let before = catalogue.capture().expect("initial capture");
        let before_registry = tools(None, Some(before.core())).expect("initial tool registry");
        std::fs::create_dir_all(user.join("new-valid")).expect("new skill directory");
        std::fs::write(
            user.join("new-valid/SKILL.md"),
            "---\nname: new-valid\ndescription: New evidence.\n---\nNew.\n",
        )
        .expect("new skill file");
        let prepared = catalogue.prepare_rescan().expect("prospective rescan");
        let prospective = prepared.capture();
        assert!(
            prospective
                .descriptors()
                .iter()
                .any(|spec| spec.name() == "new-valid")
        );
        let prospective_registry = tools(None, Some(prospective.core()))
            .expect("prospective capture supplies model tool preflight");
        assert!(
            prospective_registry
                .iter()
                .any(|tool| tool.descriptor().name == "skill")
        );

        let selected = vec!["not-a-tool".to_owned()];
        assert!(tools(Some(&selected), Some(prospective.core())).is_err());
        let after = catalogue.capture().expect("capture after failed preflight");
        assert!(std::sync::Arc::ptr_eq(&before.core(), &after.core()));
        assert_eq!(before.snapshot_digest(), after.snapshot_digest());
        let after_registry = tools(None, Some(after.core())).expect("unchanged registry");
        assert_eq!(
            before_registry
                .iter()
                .map(|tool| tool.descriptor().name)
                .collect::<Vec<_>>(),
            after_registry
                .iter()
                .map(|tool| tool.descriptor().name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn hooks_listing_renders_the_exact_runtime_arc_with_active_skill_scope() {
        let invocation = SkillInvocationId::new();
        let source = HookSource::skill_entries_with_authority(
            "skills/explain/SKILL.md",
            invocation,
            SkillSourceAuthority::User,
            [HookConfigEntry::new(
                HookLifecycle::PreToolUse,
                None,
                HookHandlerConfig::Prompt {
                    prompt: "private handler payload".to_owned(),
                    model: None,
                    timeout_seconds: None,
                },
            )],
        );
        let catalogue = HookCatalogue::admit_typed(&[], &[source], Trust::Trusted)
            .expect("admitted hook catalogue");
        let store: &'static HookCaptureStore = Box::leak(Box::new(HookCaptureStore));
        let clock: &'static HookCaptureClock = Box::leak(Box::new(HookCaptureClock));
        let sink: &'static NullSink = Box::leak(Box::new(NullSink));
        let journal = SharedJournal::new(Journal::new(
            SessionId::new(),
            store,
            clock,
            sink,
            Producer::Main,
            SessionState::default(),
        ));
        let runtime = SessionHookState::new(
            ego_core::hook::HookLifecycleDispatcher::new(HookRunner::new(), catalogue),
            journal,
            CancelToken::new(),
            Instant::now() + Duration::from_secs(30),
        )
        .bind(Arc::new(HookCaptureInvoker));
        let exact = runtime
            .hook_state()
            .catalogue_snapshot()
            .expect("runtime hook catalogue snapshot");
        let exact_again = runtime
            .hook_state()
            .catalogue_snapshot()
            .expect("runtime hook catalogue snapshot remains stable");
        assert!(Arc::ptr_eq(&exact, &exact_again));
        let rendered = crate::hooks::render(exact.as_ref(), false).expect("render hooks");
        assert!(rendered.contains(&invocation.to_string()));
        assert!(rendered.contains("skill:"));
        assert!(!rendered.contains("private handler payload"));
    }

    #[test]
    fn resume_plan_parses_one_answer_and_leaves_classification_pure() {
        let model_tool = ItemId::new();
        let (model_state, model_answer, model_turn, model_request_event) =
            pending_resume_state(QuestionTarget::ModelTool {
                tool_call: model_tool,
            });
        let model_before = model_state.clone();
        let model_plan = plan_resume_answer(&model_state, std::slice::from_ref(&model_answer))
            .expect("one model answer is planned");
        assert_eq!(model_state, model_before);
        match model_plan {
            ResumePlan::Model {
                answer,
                continuation,
                request_event,
            } => {
                assert_eq!(answer, QuestionAnswer::single("yes"));
                assert_eq!(continuation.question, model_answer.id);
                assert_eq!(continuation.turn, model_turn);
                assert_eq!(continuation.tool_call, model_tool);
                assert_eq!(request_event, model_request_event);
            }
            ResumePlan::Foreign { .. } => panic!("model answer was misclassified as foreign"),
        }

        let foreign_tool = ItemId::new();
        let continuation_id = "mcp-generation-policy-restart";
        let (foreign_state, foreign_answer, _, _) =
            pending_resume_state(QuestionTarget::ForeignContinuation {
                tool_call: foreign_tool,
                continuation_id: continuation_id.to_owned(),
            });
        let foreign_before = foreign_state.clone();
        let foreign_plan =
            plan_resume_answer(&foreign_state, std::slice::from_ref(&foreign_answer))
                .expect("one foreign answer is planned");
        assert_eq!(foreign_state, foreign_before);
        match foreign_plan {
            ResumePlan::Foreign {
                question,
                continuation_id: planned_id,
                answer,
            } => {
                assert_eq!(question, foreign_answer.id);
                assert_eq!(planned_id, continuation_id);
                assert_eq!(answer, QuestionAnswer::single("yes"));
            }
            ResumePlan::Model { .. } => panic!("foreign answer was misclassified as model"),
        }
    }

    #[test]
    fn frozen_rule_resolver_reuses_startup_layers_and_replaces_only_base_user_permissions() {
        let system = ConfigLayer::parse(
            ego_core::config::Source::System,
            "[permissions]\nallow = [\"write(src/**)\"]\ndeny = [\"read(src/**)\"]\n",
        )
        .expect("system layer");
        let user = ConfigLayer::parse(
            ego_core::config::Source::User,
            "[permissions]\nallow = [\"read(src/**)\"]\njudge = \"ollama/judge\"\n[profile.ci]\nmodel = \"ollama/ci\"\n",
        )
        .expect("user layer");
        let project = ConfigLayer::parse(
            ego_core::config::Source::Project,
            "[permissions]\nallow = [\"read(src/private/**)\"]\n",
        )
        .expect("project layer");
        let selection = GenerationPolicySelection::new(Mode::Plan, Ruleset::default());
        let resolver = FrozenRuleResolver::new(
            vec![system, user, project],
            Trust::Trusted,
            Some(("ci".to_owned(), ego_core::config::Source::Flags)),
            selection,
        );
        let before = ego_core::config::RuleSnapshot {
            target: ego_core::config::UserConfigTarget::Base,
            permissions_present: true,
            allow: vec![ego_core::permit::rule::Rule::load("read(src/**)").unwrap()],
            deny: Vec::new(),
        };
        let after = ego_core::config::RuleSnapshot {
            target: ego_core::config::UserConfigTarget::Base,
            permissions_present: true,
            allow: vec![ego_core::permit::rule::Rule::load("read(src/private/**)").unwrap()],
            deny: Vec::new(),
        };
        let result = resolver
            .resolve_rule_transaction(
                &before,
                &after,
                &[ego_core::config::RuleMutation {
                    effect: ego_core::config::ManagedRuleEffect::Allow,
                    rule: ego_core::permit::rule::Rule::load("read(src/private/**)").unwrap(),
                }],
            )
            .expect("frozen resolution");
        assert_eq!(result.next.mode, Mode::Plan);
        assert_eq!(result.next.ruleset.allow.len(), 1);
        assert_eq!(
            result.next.ruleset.allow[0].rule(),
            &ego_core::permit::rule::Rule::load("read(src/private/**)").unwrap()
        );
        assert_eq!(
            result.next.ruleset.allow[0].origins(),
            ego_core::config::OriginSet::just(ego_core::config::Source::Project)
        );
        assert_eq!(result.next.ruleset.deny.len(), 1);
        assert_eq!(
            result.next.ruleset.deny[0].rule(),
            &ego_core::permit::rule::Rule::load("read(src/**)").unwrap()
        );
        assert_eq!(
            result.next.ruleset.deny[0].origins(),
            ego_core::config::OriginSet::just(ego_core::config::Source::System)
        );

        let unchanged = GenerationPolicySelection::new(Mode::Default, Ruleset::default());
        let unknown_profile = FrozenRuleResolver::new(
            vec![
                ConfigLayer::parse(
                    ego_core::config::Source::User,
                    "[profile.ci]\nmodel = \"ollama/ci\"\n",
                )
                .expect("profile fixture"),
            ],
            Trust::Trusted,
            Some(("missing".to_owned(), ego_core::config::Source::Flags)),
            unchanged.clone(),
        );
        let error = unknown_profile
            .resolve_rule_transaction(&before, &after, &[])
            .expect_err("unknown selected profile must fail closed");
        assert!(matches!(
            error,
            crate::repl::policy_controls::PolicyControlError::Resolution { .. }
        ));
        assert_eq!(unchanged.snapshot().unwrap().mode, Mode::Default);
        assert!(unchanged.staged_snapshot().unwrap().is_none());
    }

    #[test]
    fn captured_auto_preflight_has_closed_repeatable_outcomes_without_new_io() {
        let cases = [
            (CapturedAutoPreflight::MissingJudge, false),
            (CapturedAutoPreflight::DeferredDynamic, true),
            (CapturedAutoPreflight::Valid, true),
            (
                CapturedAutoPreflight::Refused {
                    diagnostic: "captured judge cannot encode request".to_owned(),
                },
                false,
            ),
        ];
        for (state, accepted) in cases {
            let preflight = CliAutoPreflight::new(state);
            let first = preflight.validate_auto();
            let second = preflight.validate_auto();
            assert_eq!(first, second);
            assert_eq!(first.is_ok(), accepted);
            if !accepted {
                assert!(matches!(
                    first,
                    Err(crate::repl::policy_controls::PolicyControlError::JudgeUnavailable { .. })
                ));
            }
        }
    }

    #[derive(Default)]
    struct PolicyTextOutput(std::sync::Mutex<Vec<String>>, AtomicUsize);

    impl crate::output::Diagnostics for PolicyTextOutput {
        fn note(&self, _message: &str) {}
    }

    impl crate::output::TextOutput for PolicyTextOutput {
        fn write(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }

        fn line(&self, text: &str) {
            self.0.lock().unwrap().push(text.to_owned());
        }

        fn finish(&self) {
            self.1.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn real_remember_preview() -> crate::rules::RememberPreview {
        crate::rules::RuleApplication::new(
            Arc::new(UnusedStore),
            ego_core::config::UserConfigTarget::Base,
        )
        .preview_remember(crate::rules::RememberRequest::new(
            ego_core::permit::PreparedCall::new(
                "read",
                ego_core::permit::Op::Read {
                    path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
                },
            ),
            ego_core::config::ManagedRuleEffect::Allow,
            ego_core::permit::CanonPath::new("/workspace"),
        ))
        .unwrap()
    }

    fn real_rule_receipt() -> crate::repl::policy_controls::RuleMutationReceipt {
        let rule = ego_core::permit::rule::Rule::load("read(src/**)").unwrap();
        let mutation = ego_core::config::RuleMutation {
            effect: ego_core::config::ManagedRuleEffect::Allow,
            rule: rule.clone(),
        };
        crate::repl::policy_controls::RuleMutationReceipt::from_atomic_facts(
            ego_core::config::UserConfigMutationResult {
                target: ego_core::config::UserConfigTarget::Base,
                facts: ego_core::config::UserConfigMutationFacts::RuleTransaction {
                    request: Box::new(ego_core::config::UserConfigMutationFacts::Batch {
                        changes: vec![mutation.clone()],
                    }),
                    before: ego_core::config::RuleSnapshot {
                        target: ego_core::config::UserConfigTarget::Base,
                        permissions_present: true,
                        allow: vec![],
                        deny: vec![],
                    },
                    after: ego_core::config::RuleSnapshot {
                        target: ego_core::config::UserConfigTarget::Base,
                        permissions_present: true,
                        allow: vec![rule],
                        deny: vec![],
                    },
                },
                changed: true,
                publication: ego_core::config::UserConfigPublication::PublishedDurably,
            },
            vec![mutation],
            vec![ego_core::config::rule_management::RuleMutationComparison {
                stored_changed: true,
                effective_changed: true,
                authorities: vec![],
            }],
        )
        .unwrap()
    }

    #[test]
    fn handle_policy_request_renders_list_explain_and_rule_without_confirmation() {
        let output = PolicyTextOutput::default();
        let mut applied = Vec::new();
        let rules = ego_core::config::Ruleset::from_layer(
            ego_core::config::PermCfg {
                judge: None,
                allow: vec![ego_core::permit::rule::Rule::load("read(src/**)").unwrap()],
                deny: vec![],
            },
            ego_core::config::Source::Project,
        );
        let list = ego_core::config::rule_management::RuleManagement::rows(&rules);
        let list_request =
            PolicyControlRequest::Permission(crate::repl::policy_controls::PermissionInput::List);
        let list_rows = crate::rules::render_list_rows(&list);
        assert_eq!(
            handle_policy_request(
                list_request.clone(),
                |request| {
                    applied.push(request);
                    Ok(PolicyControlOutcome::Permission(
                        crate::repl::policy_controls::PermissionOutcome::List(list.clone()),
                    ))
                },
                &output,
                |_preview| unreachable!("confirmation is not requested for list"),
            )
            .expect("list request"),
            None
        );
        assert_eq!(*output.0.lock().unwrap(), list_rows);
        assert_eq!(applied, vec![list_request]);

        let call = ego_core::permit::PreparedCall::new(
            "read",
            ego_core::permit::Op::Read {
                path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
            },
        );
        let explanation = ego_core::config::rule_management::RuleManagement::explain(
            &call,
            &rules,
            std::path::Path::new("/workspace"),
        );
        let explain_request = PolicyControlRequest::Permission(
            crate::repl::policy_controls::PermissionInput::Explain {
                call: call.clone(),
                workspace: ego_core::permit::CanonPath::new("/workspace"),
            },
        );
        let explain_rows = crate::rules::render_explanation_rows(&call, &explanation);
        let explain_output = PolicyTextOutput::default();
        assert_eq!(
            handle_policy_request(
                explain_request.clone(),
                |request| {
                    applied.push(request);
                    Ok(PolicyControlOutcome::Permission(
                        crate::repl::policy_controls::PermissionOutcome::Explain(
                            explanation.clone(),
                        ),
                    ))
                },
                &explain_output,
                |_preview| unreachable!("confirmation is not requested for explain"),
            )
            .expect("explain request"),
            None
        );
        assert_eq!(*explain_output.0.lock().unwrap(), explain_rows);

        let receipt = real_rule_receipt();
        let rule_request = PolicyControlRequest::Rule(ego_core::config::RuleMutation {
            effect: ego_core::config::ManagedRuleEffect::Allow,
            rule: ego_core::permit::rule::Rule::load("read(src/**)").unwrap(),
        });
        let rule_rows = crate::rules::render_mutation_receipt_rows(
            &receipt,
            ego_core::config::ManagedRuleEffect::Allow,
        );
        let rule_output = PolicyTextOutput::default();
        assert_eq!(
            handle_policy_request(
                rule_request.clone(),
                |request| {
                    applied.push(request);
                    Ok(PolicyControlOutcome::Rule {
                        receipt: receipt.clone(),
                        staging: crate::repl::policy_controls::PolicyStageOutcome { changed: true },
                    })
                },
                &rule_output,
                |_preview| unreachable!("confirmation is not requested for rule"),
            )
            .expect("rule request"),
            None
        );
        assert_eq!(*rule_output.0.lock().unwrap(), rule_rows);
        assert_eq!(applied.len(), 3);
    }

    #[test]
    fn handle_policy_request_remember_confirmed_applies_preview_once_in_order() {
        let output = PolicyTextOutput::default();
        let mut applied = Vec::new();
        let request = PolicyControlRequest::Permission(
            crate::repl::policy_controls::PermissionInput::Remember(
                crate::rules::RememberRequest::new(
                    ego_core::permit::PreparedCall::new(
                        "read",
                        ego_core::permit::Op::Read {
                            path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
                        },
                    ),
                    ego_core::config::ManagedRuleEffect::Allow,
                    ego_core::permit::CanonPath::new("/workspace"),
                ),
            ),
        );
        let preview = real_remember_preview();
        let result = handle_policy_request(
            request,
            |request| {
                applied.push(request);
                if applied.len() == 1 {
                    Ok(PolicyControlOutcome::RememberPreview(preview.clone()))
                } else {
                    Ok(PolicyControlOutcome::Rule {
                        receipt: real_rule_receipt(),
                        staging: crate::repl::policy_controls::PolicyStageOutcome { changed: true },
                    })
                }
            },
            &output,
            |confirmed_preview| {
                assert_eq!(confirmed_preview, &preview);
                assert_eq!(
                    *output.0.lock().unwrap(),
                    crate::rules::render_remember_preview_rows(&preview)
                );
                assert_eq!(output.1.load(Ordering::SeqCst), 1);
                crate::repl::Confirmation::Confirmed
            },
        );
        assert!(result.is_ok());
        assert_eq!(output.1.load(Ordering::SeqCst), 1);
        assert_eq!(applied.len(), 2);
        assert!(matches!(
            &applied[0],
            PolicyControlRequest::Permission(
                crate::repl::policy_controls::PermissionInput::Remember(_)
            )
        ));
        assert!(matches!(
            &applied[1],
            PolicyControlRequest::Permission(
                crate::repl::policy_controls::PermissionInput::ConfirmRemember(observed)
            ) if observed == &preview
        ));
        let mut expected_output = crate::rules::render_remember_preview_rows(&preview);
        expected_output.extend(crate::rules::render_remember_result_rows(
            &ego_core::config::UserConfigTarget::Base,
            Some(&real_rule_receipt()),
        ));
        assert_eq!(*output.0.lock().unwrap(), expected_output);
    }

    #[test]
    fn handle_policy_request_declined_remember_does_not_apply_again() {
        let output = PolicyTextOutput::default();
        let preview = real_remember_preview();
        let mut applies = 0;
        let result = handle_policy_request(
            PolicyControlRequest::Permission(
                crate::repl::policy_controls::PermissionInput::Remember(
                    crate::rules::RememberRequest::new(
                        ego_core::permit::PreparedCall::new(
                            "read",
                            ego_core::permit::Op::Read {
                                path: ego_core::permit::CanonPath::new("/workspace/src/main.rs"),
                            },
                        ),
                        ego_core::config::ManagedRuleEffect::Allow,
                        ego_core::permit::CanonPath::new("/workspace"),
                    ),
                ),
            ),
            |_request| {
                applies += 1;
                Ok(PolicyControlOutcome::RememberPreview(preview.clone()))
            },
            &output,
            |_preview| crate::repl::Confirmation::Declined,
        );
        assert!(result.is_ok());
        assert_eq!(applies, 1);
        let mut expected_output = crate::rules::render_remember_preview_rows(&preview);
        expected_output.extend(crate::rules::render_remember_result_rows(
            &ego_core::config::UserConfigTarget::Base,
            None,
        ));
        assert_eq!(*output.0.lock().unwrap(), expected_output);
    }

    #[test]
    fn handle_policy_request_propagates_confirmation_terminal_lines_without_second_apply() {
        let output = PolicyTextOutput::default();
        for propagated in [
            crate::repl::Line::Ended,
            crate::repl::Line::ExternalBreak("draft".to_owned()),
        ] {
            let preview = real_remember_preview();
            let mut applies = 0;
            let result = handle_policy_request(
                PolicyControlRequest::Permission(
                    crate::repl::policy_controls::PermissionInput::Remember(
                        crate::rules::RememberRequest::new(
                            ego_core::permit::PreparedCall::new(
                                "read",
                                ego_core::permit::Op::Read {
                                    path: ego_core::permit::CanonPath::new(
                                        "/workspace/src/main.rs",
                                    ),
                                },
                            ),
                            ego_core::config::ManagedRuleEffect::Allow,
                            ego_core::permit::CanonPath::new("/workspace"),
                        ),
                    ),
                ),
                |_request| {
                    applies += 1;
                    Ok(PolicyControlOutcome::RememberPreview(preview.clone()))
                },
                &output,
                |_preview| crate::repl::Confirmation::Propagate(propagated.clone()),
            );
            assert_eq!(result.unwrap(), Some(propagated));
            assert_eq!(applies, 1);
        }
    }

    #[test]
    fn handle_policy_request_rejects_mismatched_request_and_outcome_without_rows() {
        let output = PolicyTextOutput::default();
        let result = handle_policy_request(
            PolicyControlRequest::Permission(crate::repl::policy_controls::PermissionInput::List),
            |_request| {
                Ok(PolicyControlOutcome::Mode {
                    staging: crate::repl::policy_controls::PolicyStageOutcome { changed: false },
                })
            },
            &output,
            |_preview| unreachable!("mismatched outcome cannot request confirmation"),
        );
        assert!(result.is_err());
        assert!(output.0.lock().unwrap().is_empty());
    }

    /// A chosen model has to reach the generation, not just the configuration.
    ///
    /// The seat writes into two places that spell the same fact: `slug`, which
    /// the preflight builds its generation target from, and `effective.model`,
    /// which the rest of the turn reads. When only the second moved, every
    /// control test still passed — the host was answered with its pick and the
    /// session reported it — while every generation ran the model the run
    /// started with. Nothing on the control surface can see that, so the
    /// assertion has to be on the value the preflight is handed.
    #[test]
    fn a_chosen_model_moves_the_name_the_generation_is_built_from() {
        use ego_core::config::{Mode, ModelRef, Trust};
        use ego_core::control::{ControlSelect, SessionControlState};

        let mut effective =
            ego_core::config::resolve(&[], Trust::Trusted, None).expect("empty config");
        let mut slug = "started-with".to_owned();
        let state = SessionControlState {
            model: ControlSelect {
                current: Some(ModelRef("started-with".to_owned())),
                chosen: Some(ModelRef("picked".to_owned())),
                available: Vec::new(),
            },
            effort: ControlSelect {
                current: Some(ThinkingLevel::High),
                chosen: Some(ThinkingLevel::High),
                available: Vec::new(),
            },
            mode: ControlSelect {
                current: Some(Mode::Plan),
                chosen: Some(Mode::Plan),
                available: Vec::new(),
            },
            sandbox: ControlSelect {
                current: Some(ego_core::permit::Confinement::Workspace),
                chosen: None,
                available: Vec::new(),
            },
        };

        apply_seat(&state, &mut effective, &mut slug);

        assert_eq!(slug, "picked", "the name the generation is built from");
        assert_eq!(effective.model, Some(ModelRef("picked".to_owned())));
        assert_eq!(effective.thinking, Some(ThinkingLevel::High));
        assert_eq!(effective.mode, Some(Mode::Plan));
    }

    /// What was not chosen is left exactly as configuration resolved it.
    /// Inheriting a default is not a decision, and a run that overwrote the
    /// resolved name with an unchosen one would claim a pick nobody made.
    #[test]
    fn an_unchosen_control_leaves_what_configuration_resolved_alone() {
        use ego_core::config::{Mode, ModelRef, Trust};
        use ego_core::control::{ControlSelect, SessionControlState};

        let mut effective =
            ego_core::config::resolve(&[], Trust::Trusted, None).expect("empty config");
        effective.model = Some(ego_core::config::ModelRef("configured".to_owned()));
        let mut slug = "configured".to_owned();
        let state = SessionControlState {
            model: ControlSelect {
                current: Some(ModelRef("configured".to_owned())),
                chosen: None,
                available: Vec::new(),
            },
            effort: ControlSelect {
                current: Some(ThinkingLevel::Off),
                chosen: None,
                available: Vec::new(),
            },
            mode: ControlSelect {
                current: Some(Mode::Default),
                chosen: None,
                available: Vec::new(),
            },
            sandbox: ControlSelect {
                current: Some(ego_core::permit::Confinement::Workspace),
                chosen: None,
                available: Vec::new(),
            },
        };

        apply_seat(&state, &mut effective, &mut slug);

        assert_eq!(slug, "configured");
        assert_eq!(
            effective.model,
            Some(ego_core::config::ModelRef("configured".to_owned()))
        );
    }
}
