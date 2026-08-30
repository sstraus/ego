//! One headless run, start to exit code.
//!
//! Everything this module decides, it decides once and hands to something
//! else: the exit code is `RunTermination::exit_code`, the projection is
//! `project_public`, the totals are `SessionState`'s. What is genuinely the
//! binary's own is the *order* — config, then instructions, then a model, then
//! a store, then turns — and the two things only a process can do: notice a
//! signal, and notice a deadline.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::time::Duration;

use ego_core::ceiling::{Budget, Ceilings};
use ego_core::config::{
    Effective, Limits, Mode, ModelRef, Posture, Selection, SystemPrompt, Trust, UserConfigStore,
    resolve, system_prompt,
};
use ego_core::cost::CurrencyCode;
use ego_core::event::domain::CancelCause;
use ego_core::event::domain::EventKind;
use ego_core::event::id::{Producer, SessionId};
use ego_core::evidence::{
    ConfigProvenance, EvidenceAdmissionService, EvidenceError, EvidenceRecord, EvidenceWrite,
    RunContextEvidence, RunInvocationId,
};
use ego_core::generation_model_source::{
    GenerationModelError, GenerationModelErrorClass, GenerationProfile, GenerationRole,
};
use ego_core::generator::capabilities::ModelCapabilities;
use ego_core::generator::types::{ImageRef, ThinkingLevel, ToolDescriptor};
use ego_core::permit::judge::{JudgeConfig, JudgeInput};
use ego_core::permit::{
    CanonPath, Change, Escalation, ExecCaps, ExecObservation, HumanApprover, Op, OpName,
    PermissionDecision, PermissionRequest, PolicyEngine, PreparedCall, RunConfinement,
};
use ego_core::public::{PUBLIC_EVENT_VERSION, PublicEvent, QuestionReport, project_public};
use ego_core::question::{AskQuestionTool, Elicitor, QuestionOutcome};
use ego_core::run::{
    CancelReason, DeniedBy, RecoveryEvidence, RunExecutionBindings, RunExecutor, RunExecutorError,
    RunHandle, RunId, RunPreparation, RunPrepared, RunSettlement, RunStart, RunTermination, Signal,
};
use ego_core::sink::{EventSink, NullSink};
use ego_core::state::{ItemKind, QuestionContinuation, SessionState};
use ego_core::store::run_artifact::{PublishedRunArtifact, RunArtifactStore};
use ego_core::store::session_store::SessionMeta;
use ego_core::turn::{
    AdmittedToolSource, CancelToken, GenerationRuntime, InteractiveControl, Journal, JudgeContext,
    QueueStatus, RegisteredTool, Session, ToolEffectPorts, Tools, TurnConfig, TurnLoop,
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
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::budget;
use crate::cli::{Args, ForkArgs, ModelsArgs, Output, ResumeAnswer, SessionChoice, UsageError};
use crate::model_picker::ModelPicker;
use crate::models;
use crate::output::{
    Diagnostics, InteractiveOutput, JsonlSink, Report, Stderr, Stdout, TextOutput, TextSink, note,
};
use crate::provider_http::{ProviderHttpAdapter, run_effect_ports};
use crate::render::caps::{Capabilities, Environment, OscProfile, Terminal};
use crate::render::osc::{Osc, Progress, State};
use crate::session::{self, ActiveSession, Opened};
use crate::{attachment, config, model, prompt, repl, schema};

/// The currency a session settles in.
///
/// One constant and not a flag, because `--currency` is not in `CLI.md` §3 and
/// a currency nobody can state is worse than a stated one: every quote this
/// build can produce is either zero (a local daemon) or supplied alongside its
/// own currency.
const SETTLEMENT: &str = "USD";

/// Validate and apply the sole answer batch against one already-opened
/// durable session. Validation is complete before the first journal append.
pub fn apply_resume_answers_once(
    opened: &mut Opened<'_>,
    answers: &[ResumeAnswer],
) -> Result<QuestionContinuation, RunTermination> {
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

    let state = opened.state();
    let supplied = &answers[0];
    let answer = state
        .answer_pending(supplied.id, &supplied.json)
        .map_err(|error| usage(error.to_string()))?;
    let continuation = state
        .resume_question(supplied.id, answer.clone())
        .map_err(|error| usage(error.to_string()))?;
    let pending = state
        .pending_question()
        .expect("answer validation found the pending question");
    let request_event = pending.request_event();

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
            Some(supplied.id),
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
    registry: Arc<McpRegistryController>,
    evidence: Arc<EvidenceAdmissionService>,
) -> Ended {
    run_process(args, out, cancel, registry, evidence).await
}

async fn run_process(
    args: Args,
    out: &Stdout,
    cancel: CancelToken,
    mcp_registry: Arc<McpRegistryController>,
    evidence: Arc<EvidenceAdmissionService>,
) -> Ended {
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
    announce.opening(args.prompt.as_deref());

    let ended = run_with_registry(args, |_| sink, cancel, mcp_registry, evidence).await;

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
    registry: Arc<McpRegistryController>,
    evidence: Arc<EvidenceAdmissionService>,
) -> Ended {
    interactive_process(args, out, cancel, registry, evidence).await
}

async fn interactive_process(
    args: Args,
    out: &Stdout,
    cancel: CancelToken,
    mcp_registry: Arc<McpRegistryController>,
    evidence: Arc<EvidenceAdmissionService>,
) -> Ended {
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
    let services = InteractiveServices {
        mcp_registry,
        evidence,
    };

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
    mcp_registry: Arc<McpRegistryController>,
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
    };
    let between = || text_sink.finish();
    let ended = match execute(
        args,
        |_| &text_sink as &dyn EventSink,
        cancel,
        RunHost {
            driver: Driver::Interactive {
                seat: interactive_seat,
                between: &between,
            },
            elicitor: Some(elicitor),
            status: Some(status),
            diagnostics: output,
            mcp_registry: services.mcp_registry,
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
    mcp_registry: Arc<McpRegistryController>,
    evidence: Arc<EvidenceAdmissionService>,
) -> Ended {
    match execute(
        args,
        destination,
        cancel,
        RunHost {
            driver: Driver::Once,
            elicitor: None,
            status: None,
            diagnostics: &Stderr,
            mcp_registry,
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
fn tools(selected: Option<&Vec<String>>) -> Result<Vec<RegisteredTool>, UsageError> {
    let mut all: Vec<_> = builtins().into_iter().map(RegisteredTool::Effect).collect();
    all.push(RegisteredTool::HumanInput(Arc::new(AskQuestionTool)));
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
    effective: Effective,
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
    let effective = resolve(&layers, trust, config::profile(args).as_deref())
        .map_err(|err| UsageError(err.to_string()))?;
    finish_rooted_settings(args, cwd, effective, trust, diagnostics)
}

fn frozen_rooted_settings(
    args: &Args,
    cwd: PathBuf,
    frozen: &config::FrozenRunConfig,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
    let effective = frozen.resolve(args)?;
    finish_rooted_settings(args, cwd, effective, Trust::Untrusted, diagnostics)
}

fn finish_rooted_settings(
    args: &Args,
    cwd: PathBuf,
    effective: Effective,
    trust: Trust,
    diagnostics: &dyn Diagnostics,
) -> Result<Settings, UsageError> {
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
        effective,
    })
}

/// What a turn is made of: the prompt, its system half, the tools on offer,
/// the schema the answer must satisfy, and the name of the model to ask.
struct Materials {
    system: Option<SystemPrompt>,
    registry: Vec<RegisteredTool>,
    compiled: Option<schema::Schema>,
    text: String,
    slug: String,
}

/// Everything a turn needs that comes from a file, a flag or stdin.
///
/// Gathered before a store is chosen and before a model is resolved, so a run
/// naming a schema that will not compile or a tool this build does not have
/// refuses without creating a session or reaching a network.
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
    let registry = tools(args.tools.as_ref())?;
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
        compiled,
        text,
        slug,
    })
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
/// [`execute`], because its writer lease is the authority that durably admits
/// the run context before model or MCP effects begin.
fn chosen(args: &Args, root: &Path) -> Result<Chosen, UsageError> {
    let store_root = rooted_path(root, session::store_root(args.session_dir.as_ref())?);
    // The same `root` that derives a named session's id below, so "the session
    // called build in this workspace" and "the sessions belonging to this
    // workspace" cannot disagree about which workspace that is.
    let store = session::jsonl(store_root, root.to_path_buf());
    let id = session::id_for(&args.session, root, &store)?;

    Ok(Chosen { store, id })
}

/// Prove the fixed judge generation envelope after run-context admission.
///
/// Dynamic authority facts occupy one user-text block and cannot change the
/// request's tool, thinking, parallelism or output-ceiling requirements. A
/// canonical write-shaped input therefore exercises the same provider-facing
/// shape without sending a generation or pretending that the provider was
/// contacted. The exact in-turn request is checked again by `TurnLoop`.
fn preflight_judge(
    judge: &GenerationProfile,
    confinement: &RunConfinement,
    exec_caps: ExecCaps,
) -> Result<(), RunTermination> {
    let path = confinement.workspace().clone();
    let call = PreparedCall::new(
        "write",
        vec![Op::Write {
            path: path.clone(),
            change: Change::Whole(Vec::new()),
        }],
    );
    let escalation = Escalation::ModeAsks {
        mode: Mode::Auto,
        op: OpName::Write,
        path,
    };
    let request =
        JudgeInput::new(&call, confinement, exec_caps, &escalation).request(&JudgeConfig {
            identity: judge.identity.clone(),
            request_model: judge.request_model.clone(),
            max_output_tokens: judge.output_ceiling(),
        });
    let gaps = judge.capabilities.request_gaps(&request);
    if !gaps.is_empty() {
        return Err(RunTermination::UsageError {
            message: format!(
                "judge model `{}` is incompatible with the fixed judge request: {}",
                judge.identity.0.as_str(),
                gaps.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        });
    }
    judge
        .generator
        .ensure_expressible(&request)
        .map_err(|error| RunTermination::UsageError {
            message: format!(
                "judge model `{}` cannot encode the fixed judge request: {error}",
                judge.identity.0.as_str()
            ),
        })
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
    judge_enabled: bool,
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
    let (proof_ports, proof_observation) =
        effect_scope(confinement, &proof_evidence, cancel, run_deadline).await?;
    let http = Arc::new(ProviderHttpAdapter::new(proof_ports)) as Arc<dyn ego_ai::http::Http>;

    let mode = effective.mode.unwrap_or(Mode::Default);
    let judge_identity = if mode == Mode::Auto {
        Some(
            effective
                .judge
                .clone()
                .ok_or_else(|| RunTermination::UsageError {
                    message: "`auto` mode requires `[permissions] judge = \"provider/model\"`"
                        .to_string(),
                })?,
        )
    } else {
        None
    };
    let source = Arc::new(model::CliGenerationModelSource::new(
        ego_core::config::ModelRef(slug),
        effective.thinking.unwrap_or(ThinkingLevel::Off),
        judge_identity,
        effective.ceilings.clone(),
        effective.auth.clone(),
    ));
    let selection = source.selection();
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
        debug_assert!(
            status.picker.is_some(),
            "the run retains the installed picker"
        );
    }
    let bound = source.bind(Arc::clone(&http), cancel.clone(), run_deadline);
    let main = bound
        .resolve(GenerationRole::Main)
        .await
        .map_err(generation_refusal)?;
    let capabilities = main.capabilities.clone();
    let judge_enabled = mode == Mode::Auto;
    if judge_enabled {
        let judge = bound
            .resolve(GenerationRole::Judge)
            .await
            .map_err(generation_refusal)?;
        preflight_judge(&judge, confinement, proof_observation.capabilities())?;
    }

    Ok(GenerationPreflight {
        source,
        selection,
        capabilities,
        judge_enabled,
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
) -> Result<(Arc<RunEffectPorts>, ExecObservation), RunTermination> {
    run_effect_ports(
        confinement.clone(),
        evidence.clone(),
        cancel.clone(),
        run_deadline,
    )
    .await
    .map_err(|err| match &err {
        ProcessError::MissingCapabilities(_) | ProcessError::Backend(_) => {
            RunTermination::UsageError {
                message: err.to_string(),
            }
        }
        ProcessError::Cancelled => cancelled_termination(cancel),
        _ => RunTermination::Failed {
            message: err.to_string(),
        },
    })
}

async fn acquire_attachments(
    specs: &[String],
    source: &Arc<model::CliGenerationModelSource>,
    ports: &Arc<RunEffectPorts>,
    cancel: &CancelToken,
    run_deadline: Option<std::time::Instant>,
    cwd: &Path,
    blobs: &dyn ego_core::store::blob::BlobStore,
) -> Result<Vec<ImageRef>, RunTermination> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(ports))) as Arc<dyn ego_ai::http::Http>;
    let profile = source
        .bind(http, cancel.clone(), run_deadline)
        .resolve(GenerationRole::Main)
        .await
        .map_err(generation_refusal)?;
    attachment::acquire(
        specs,
        &profile.capabilities,
        profile.identity.0.as_str(),
        &profile.request_model,
        cwd,
        ports.as_ref(),
        blobs,
    )
    .await
    .map_err(|error| RunTermination::UsageError {
        message: error.to_string(),
    })
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
    Prompt(&'a str),
    Resumed(ego_core::event::id::TurnId),
}

#[allow(clippy::too_many_arguments)]
async fn drive_headless_turn(
    active: &mut ActiveSession<'_>,
    opening: SessionState,
    clock: &SystemClock,
    sink: &dyn EventSink,
    cancel: &CancelToken,
    source: Arc<model::CliGenerationModelSource>,
    admission: Arc<EvidenceAdmissionService>,
    invocation: RunInvocationId,
    effective: &Effective,
    config: TurnConfig,
    turns: &Turns<'_>,
    registry: &[RegisteredTool],
    admitted_tools: Option<Arc<dyn AdmittedToolSource>>,
    policy: &PolicyEngine,
    cwd: &Path,
    confinement: &RunConfinement,
    run_deadline: Option<std::time::Instant>,
    elicitor: Option<Arc<dyn Elicitor>>,
    judge_enabled: bool,
    attachment_specs: &[String],
    input: HeadlessInput<'_>,
    diagnostics: &dyn Diagnostics,
) -> Result<(RunTermination, SessionState, Value), RunTermination> {
    let currency = CurrencyCode::new(SETTLEMENT).expect("a controlled currency");
    let mut gate = budget::gate(&effective.ceilings, effective.clamped.money, currency);
    let (ports, observation) =
        effect_scope(confinement, active.evidence(), cancel, run_deadline).await?;
    let images = acquire_attachments(
        attachment_specs,
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
    let session = Session {
        id: active.id(),
        state: opening,
        store: active.store(),
        evidence: active.run_evidence(),
        clock,
        sink,
        cancel: cancel.clone(),
    };
    let runtime = GenerationRuntime {
        source: source.bind(http, cancel.clone(), run_deadline),
        gate: &mut gate,
        config,
        admission,
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
    let tools = match admitted_tools {
        Some(source) => Tools::from_registry_with_source(
            registry,
            source,
            policy,
            effect_ports,
            CanonPath::new(cwd),
        )
        .map_err(|error| RunTermination::Failed {
            message: format!("the admitted MCP tool catalogue is invalid: {error}"),
        })?,
        None => Tools::from_registry(registry, policy, effect_ports, CanonPath::new(cwd)),
    };
    let loop_ = TurnLoop::new(session, runtime, tools);
    let loop_ = match active.replayed() {
        Some(replayed) => loop_.with_ancestor_conversation(replayed.conversation().clone()),
        None => loop_,
    };
    let loop_ = match elicitor {
        Some(elicitor) => loop_.with_elicitor(elicitor),
        None => loop_,
    };
    let mut loop_ = if judge_enabled {
        loop_.with_judge(JudgeContext {
            confinement: confinement.clone(),
            exec_caps: execution_capabilities,
        })
    } else {
        loop_
    }
    .with_execution_observation(observation);
    let (termination, result) = match input {
        HeadlessInput::Prompt(prompt) => {
            turns
                .drive_with_images(&mut loop_, prompt, images, cancel, diagnostics)
                .await
        }
        HeadlessInput::Resumed(turn) => {
            turns
                .drive_resumed(&mut loop_, turn, cancel, diagnostics)
                .await
        }
    };
    Ok((termination, loop_.into_state(), result))
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
    max_turns: Option<u32>,
    run_deadline: Option<std::time::Instant>,
    root: PathBuf,
    cwd: PathBuf,
    confinement: RunConfinement,
    effective: Effective,
    system: Option<SystemPrompt>,
    registry: Vec<RegisteredTool>,
    compiled: Option<schema::Schema>,
    slug: String,
    run_context: RunContextEvidence,
    admission: Arc<EvidenceAdmissionService>,
}

impl CliPrepared {
    async fn prepare(
        mut args: Args,
        store: Arc<JsonlStore>,
        artifacts: Option<Arc<dyn RunArtifactStore>>,
        request: &RunPreparation,
        permit: Option<OwnedSemaphorePermit>,
        frozen: Option<&config::FrozenRunConfig>,
        admission: Arc<EvidenceAdmissionService>,
    ) -> Result<RunPrepared<Self>, RunTermination> {
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
        if let Some(budget) = request.selections.budget() {
            args.budget = Some(run_budget_spec(budget)?);
        }
        if let Some(profile) = request.selections.profile() {
            args.profile = Some(profile.to_owned());
        }
        let root = request.root.as_path().to_path_buf();
        let settings = match frozen {
            Some(frozen) => frozen_rooted_settings(&args, root, frozen, &Stderr),
            None => rooted_settings(&args, root, |_| Trust::Untrusted, &Stderr),
        }
        .map_err(usage)?;
        let Materials {
            system,
            registry,
            compiled,
            text: _,
            slug,
        } = prepared_materials(&settings, &args, String::new(), &Stderr).map_err(usage)?;
        let Settings {
            run_deadline,
            root,
            cwd,
            confinement,
            effective,
        } = settings;
        let schema_retries = args.schema_retries;
        let max_turns = args.max_turns;
        let instructions = system
            .as_ref()
            .map(|prompt| prompt.sources.clone())
            .unwrap_or_default();
        let config_provenance = ConfigProvenance::resolved(args.profile.clone());
        let run_context = RunContextEvidence::new(
            request.run_id.session_id(),
            request.invocation,
            request.run_id.to_string(),
            config_provenance,
        )
        .with_instructions(instructions);

        Ok(RunPrepared::new(
            ModelRef(slug.clone()),
            Self {
                _permit: permit,
                store,
                artifacts,
                run_id: request.run_id,
                session_id: request.run_id.session_id(),
                schema_retries,
                max_turns,
                run_deadline,
                root,
                cwd,
                confinement,
                effective,
                system,
                registry,
                compiled,
                slug,
                run_context,
                admission,
            },
        ))
    }

    async fn submit(
        self,
        prompt: &str,
        bindings: RunExecutionBindings,
        cancel: &CancelToken,
    ) -> Result<RunSettlement, RunExecutorError> {
        let run_id = self.run_id;
        let artifacts = self.artifacts.clone();
        let clock = SystemClock;
        let opened = Opened::Durable(Box::new(
            session::open(self.store.as_ref(), &self.session_id, &clock)
                .map_err(|error| RunExecutorError::new(error.to_string()))?,
        ));
        if let Some(note_text) = opened.repair_note() {
            Stderr.note(&note_text);
        }
        let mut no_status = None;
        let GenerationPreflight {
            source,
            selection: _,
            capabilities: model_capabilities,
            judge_enabled,
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
        let opening = opened.state();
        drop(proof_directory);
        drop(proof_evidence);
        let mut active = ActiveSession::durable(opened, opening.clone(), EVIDENCE_CAP)
            .map_err(|error| RunExecutorError::new(error.to_string()))?;
        let mcp_errors = bindings.mcp_errors().to_vec();
        let admitted_tools = bindings.admitted_tools();
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
        let policy = PolicyEngine::new(
            CanonPath::new(&self.root),
            mode,
            self.effective.permissions.clone(),
        );
        let config = TurnConfig {
            system: self.system,
            token_ceiling: self.effective.ceilings.tokens,
        };
        let turns = Turns {
            schema: self.compiled.as_ref(),
            retries: self.schema_retries,
            max_turns: self.max_turns,
            deadline: self.run_deadline,
        };
        let admission = Arc::clone(&self.admission);
        let invocation = self.run_context.invocation();
        let (termination, state, result) = match drive_headless_turn(
            &mut active,
            opening,
            &clock,
            sink.as_ref(),
            cancel,
            source,
            admission,
            invocation,
            &self.effective,
            config,
            &turns,
            &self.registry,
            admitted_tools,
            &policy,
            &self.cwd,
            &self.confinement,
            self.run_deadline,
            bindings.elicitor,
            judge_enabled,
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
            artifacts
                .publish(run_id, public_result_artifact(result)?)
                .map_err(|error| RunExecutorError::new(error.to_string()))?;
        }
        Ok(settlement)
    }
}

fn public_result_artifact(result: Value) -> Result<PublishedRunArtifact, RunExecutorError> {
    match result {
        Value::String(text) => Ok(PublishedRunArtifact::output(
            "text/plain",
            text.into_bytes(),
        )),
        structured => serde_json::to_vec(&structured)
            .map(|bytes| PublishedRunArtifact::output("application/json", bytes))
            .map_err(|error| RunExecutorError::new(error.to_string())),
    }
}

fn run_executor_error(termination: RunTermination) -> RunExecutorError {
    RunExecutorError::new(match termination {
        RunTermination::UsageError { message }
        | RunTermination::Provider { message }
        | RunTermination::Failed { message } => message,
        other => format!(
            "the prepared run stopped before submission: {}",
            other.exit_reason()
        ),
    })
}

fn settlement(termination: RunTermination) -> Result<RunSettlement, RunExecutorError> {
    match termination {
        RunTermination::Done | RunTermination::TaskFailed | RunTermination::MaxTurns => {
            Ok(RunSettlement::completed())
        }
        RunTermination::PermissionRequired { action, by } => {
            Ok(RunSettlement::permission_required(action, by))
        }
        RunTermination::TimedOut => Ok(RunSettlement::cancelled(CancelReason::Timeout)),
        RunTermination::Signalled(Signal::Int) => Ok(RunSettlement::cancelled(CancelReason::User)),
        RunTermination::Signalled(Signal::Term) => Ok(RunSettlement::cancelled(CancelReason::Host)),
        RunTermination::BudgetExceeded => Ok(RunSettlement::completed()),
        RunTermination::QuestionRequired { question } => Err(RunExecutorError::new(format!(
            "question {} still requires an answer",
            question.id
        ))),
        termination => Err(run_executor_error(termination)),
    }
}

#[derive(Default)]
struct CliRunAuthority {
    state: Mutex<CliRunAuthorityState>,
}

#[derive(Default)]
struct CliRunAuthorityState {
    active: Option<CancelToken>,
    pending_interrupt: bool,
    settlement: Option<RunSettlement>,
    observers: Vec<Waker>,
}

impl CliRunAuthority {
    fn begin(&self) -> CancelToken {
        let mut state = self.state.lock().expect("CLI run authority");
        let cancel = CancelToken::new();
        if state.pending_interrupt {
            state.pending_interrupt = false;
            cancel.cancel(CancelCause::User);
        }
        state.active = Some(cancel.clone());
        state.settlement = None;
        cancel
    }

    fn interrupt(&self) {
        let mut state = self.state.lock().expect("CLI run authority");
        match state.active.as_ref() {
            Some(cancel) => cancel.cancel(CancelCause::User),
            None => state.pending_interrupt = true,
        }
    }

    fn complete(&self, settlement: RunSettlement) {
        let observers = {
            let mut state = self.state.lock().expect("CLI run authority");
            state.active = None;
            state.settlement = Some(settlement);
            std::mem::take(&mut state.observers)
        };
        for observer in observers {
            observer.wake();
        }
    }

    async fn observe(&self) -> RunSettlement {
        futures::future::poll_fn(|context| {
            let mut state = self.state.lock().expect("CLI run authority");
            match state.settlement.clone() {
                Some(settlement) => std::task::Poll::Ready(settlement),
                None => {
                    if !state
                        .observers
                        .iter()
                        .any(|observer| observer.will_wake(context.waker()))
                    {
                        state.observers.push(context.waker().clone());
                    }
                    std::task::Poll::Pending
                }
            }
        })
        .await
    }
}

/// Production executor adapter for controller-owned headless turns.
pub struct CliRunExecutor {
    args: Args,
    store: Arc<JsonlStore>,
    concurrency: Option<Arc<Semaphore>>,
    frozen: Option<Arc<config::FrozenRunConfig>>,
    artifacts: Option<Arc<dyn RunArtifactStore>>,
    admission: Arc<EvidenceAdmissionService>,
}

#[derive(Clone)]
pub(crate) struct CliMcpRunPolicy {
    pub(crate) root: ego_core::run::RunRoot,
    pub(crate) model: ModelRef,
    pub(crate) thinking: ThinkingLevel,
    pub(crate) mode: Mode,
    pub(crate) ceilings: Ceilings,
    pub(crate) profile: Option<String>,
}

fn protocol_args(session_dir: Option<PathBuf>) -> Args {
    Args {
        prompt: None,
        agent_file: None,
        attach: Vec::new(),
        stdin: crate::cli::Stdin::Ambient,
        model: None,
        offline: false,
        sandbox: None,
        thinking: None,
        system: None,
        append_system: None,
        instructions: None,
        bare: false,
        mode: None,
        allow: Vec::new(),
        deny: Vec::new(),
        answers: Vec::new(),
        session: SessionChoice::New,
        session_dir,
        output: Output::Json,
        schema: None,
        schema_retries: 0,
        budget: None,
        max_turns: None,
        timeout: None,
        tools: None,
        mcp: Vec::new(),
        directory: None,
        set: Vec::new(),
        profile: None,
        quiet: false,
        no_color: false,
    }
}

impl CliRunExecutor {
    /// Build the fixed headless inputs for the ACP host once, before admission.
    pub fn for_acp(
        session_dir: Option<PathBuf>,
        store: Arc<JsonlStore>,
        admission: Arc<EvidenceAdmissionService>,
    ) -> Self {
        Self {
            args: protocol_args(session_dir),
            store,
            concurrency: None,
            frozen: None,
            artifacts: None,
            admission,
        }
    }

    pub(crate) fn for_mcp(
        root: ego_core::run::RunRoot,
        session_dir: Option<PathBuf>,
        store: Arc<JsonlStore>,
        artifacts: Arc<dyn RunArtifactStore>,
        max_concurrent_runs: NonZeroUsize,
        admission: Arc<EvidenceAdmissionService>,
    ) -> Result<(Self, CliMcpRunPolicy), UsageError> {
        let args = protocol_args(session_dir);
        let frozen = Arc::new(config::FrozenRunConfig::capture(
            root.as_path(),
            &args,
            Trust::Untrusted,
        )?);
        let effective = frozen.resolve(&args)?;
        let model = effective.model.clone().ok_or_else(|| {
            UsageError("mcp-server requires an effective configured model".to_owned())
        })?;
        if effective.ceilings.money.is_none() && effective.ceilings.tokens.is_none() {
            return Err(UsageError(
                "mcp-server requires at least one effective budget ceiling".to_owned(),
            ));
        }
        let policy = CliMcpRunPolicy {
            root,
            model,
            thinking: effective.thinking.unwrap_or(ThinkingLevel::Off),
            mode: effective.mode.unwrap_or(Mode::Default),
            ceilings: effective.ceilings,
            profile: frozen.profile().map(str::to_owned),
        };
        Ok((
            Self {
                args,
                store,
                concurrency: Some(Arc::new(Semaphore::new(max_concurrent_runs.get()))),
                frozen: Some(frozen),
                artifacts: Some(artifacts),
                admission,
            },
            policy,
        ))
    }

    fn authority(handle: &RunHandle) -> Result<Arc<CliRunAuthority>, RunExecutorError> {
        handle
            .authority::<Arc<CliRunAuthority>>()
            .cloned()
            .ok_or_else(|| RunExecutorError::new("run handle carries another executor authority"))
    }
}

impl RunExecutor for CliRunExecutor {
    type Prepared = CliPrepared;

    fn prepare<'a>(
        &'a self,
        request: &'a RunPreparation,
    ) -> futures::future::BoxFuture<'a, Result<RunPrepared<Self::Prepared>, RunExecutorError>> {
        Box::pin(async move {
            let permit = match &self.concurrency {
                Some(concurrency) => {
                    Some(Arc::clone(concurrency).try_acquire_owned().map_err(|_| {
                        RunExecutorError::new("run concurrency ceiling is unavailable")
                    })?)
                }
                None => None,
            };
            CliPrepared::prepare(
                self.args.clone(),
                Arc::clone(&self.store),
                self.artifacts.clone(),
                request,
                permit,
                self.frozen.as_deref(),
                Arc::clone(&self.admission),
            )
            .await
            .map_err(run_executor_error)
        })
    }

    fn admit_run_context<'a>(
        &'a self,
        prepared: &'a Self::Prepared,
    ) -> ego_core::run::RunFuture<'a, Result<EvidenceWrite, EvidenceError>> {
        Box::pin(async move {
            let key = EvidenceRecord::RunContext(prepared.run_context.clone()).key();
            let clock = SystemClock;
            let opened = session::open(self.store.as_ref(), &prepared.session_id, &clock).map_err(
                |error| EvidenceError::Store {
                    key,
                    diagnostic: error.to_string(),
                },
            )?;
            self.admission
                .admit_run_context(&opened.writer, prepared.run_context.clone())
        })
    }

    fn start<'a>(
        &'a self,
        request: &'a RunStart,
    ) -> futures::future::BoxFuture<'a, Result<RunHandle, RunExecutorError>> {
        Box::pin(async move {
            Ok(RunHandle::new(
                request.run_id,
                Arc::new(CliRunAuthority::default()),
            ))
        })
    }

    fn recover<'a>(
        &'a self,
        _run: &'a ego_core::run::RunId,
    ) -> futures::future::BoxFuture<'a, Result<RecoveryEvidence, RunExecutorError>> {
        Box::pin(async { Ok(RecoveryEvidence::NotRecoverable) })
    }

    fn observe<'a>(
        &'a self,
        handle: &'a RunHandle,
        _bindings: RunExecutionBindings,
    ) -> futures::future::BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(async move { Ok(Self::authority(handle)?.observe().await) })
    }

    fn submit<'a>(
        &'a self,
        handle: &'a RunHandle,
        prompt: &'a str,
        prepared: Self::Prepared,
        bindings: RunExecutionBindings,
    ) -> futures::future::BoxFuture<'a, Result<RunSettlement, RunExecutorError>> {
        Box::pin(async move {
            let authority = Self::authority(handle)?;
            let cancel = authority.begin();
            let result = prepared.submit(prompt, bindings, &cancel).await;
            match result.as_ref() {
                Ok(settlement) => authority.complete(settlement.clone()),
                Err(_) => authority.complete(RunSettlement::Uncertain),
            }
            result
        })
    }

    fn interrupt<'a>(
        &'a self,
        handle: &'a RunHandle,
    ) -> futures::future::BoxFuture<'a, Result<(), RunExecutorError>> {
        Box::pin(async move {
            Self::authority(handle)?.interrupt();
            Ok(())
        })
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

/// Everything before the first turn.
///
/// What is left here after the four steps above is the *order* they happen in,
/// which is the part that is genuinely this function's: settings before
/// session choice before materials because a missing `resume` target is true
/// independently of model configuration, materials before the provider because
/// a bad `--tools` must not reach a network, then the writer lease and durable
/// run-context evidence before MCP reconciliation or provider resolution.
async fn execute<S: EventSink>(
    args: Args,
    destination: impl FnOnce(&ModelCapabilities) -> S,
    cancel: CancelToken,
    host: RunHost<'_>,
) -> Result<Ended, RunTermination> {
    let RunHost {
        driver,
        elicitor,
        mut status,
        diagnostics,
        mcp_registry,
        evidence,
    } = host;
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };

    let settings =
        settings(&args, |root| run_trust(&args, &driver, root), diagnostics).map_err(usage)?;
    let selected_mcp =
        config::selected_mcp(&args, &settings.effective, &settings.cwd).map_err(usage)?;
    let selected_mcp = ego_tools::mcp::materialize_admissions(selected_mcp);
    let refresh = ego_mcp::McpRegistryRefresh::new(
        selected_mcp,
        ego_mcp::RegistryTrigger::ConfigChanged,
        ego_mcp::RegistryScope::All,
    )
    .map_err(|error| RunTermination::Failed {
        message: error.to_string(),
    })?;
    let Chosen {
        store: jsonl_store,
        id: session_id,
    } = chosen(&args, &settings.root).map_err(usage)?;
    let clock = SystemClock;
    let early_opened = if args.answers.is_empty() {
        None
    } else {
        Some(Opened::Durable(Box::new(
            session::open(&jsonl_store, &session_id, &clock).map_err(|err| {
                RunTermination::Failed {
                    message: err.to_string(),
                }
            })?,
        )))
    };
    let Materials {
        system,
        mut registry,
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

    let Settings {
        run_deadline,
        root,
        cwd,
        confinement,
        effective,
    } = settings;
    let mode = effective.mode.unwrap_or(Mode::Default);

    let currency = CurrencyCode::new(SETTLEMENT).expect("a constant this file controls");
    let mut opened = match early_opened {
        Some(opened) => opened,
        None => Opened::Durable(Box::new(
            session::open(&jsonl_store, &session_id, &clock).map_err(|err| {
                RunTermination::Failed {
                    message: err.to_string(),
                }
            })?,
        )),
    };
    if let Some(note_text) = opened.repair_note() {
        diagnostics.note(&note_text);
    }

    let invocation = RunInvocationId::new();
    let instruction_descriptors = system
        .as_ref()
        .map(|prompt| prompt.sources.clone())
        .unwrap_or_default();
    let context = RunContextEvidence::new(
        session_id,
        invocation,
        session_id.to_string(),
        ConfigProvenance::resolved(args.profile.clone()),
    )
    .with_instructions(instruction_descriptors);
    let Opened::Durable(opened_session) = &opened;
    match evidence.admit_run_context(&opened_session.writer, context) {
        Ok(EvidenceWrite::PublishedWithProjectionWarning { diagnostic }) => {
            diagnostics.note(&format!(
                "run evidence was durable but its manifest projection failed: {diagnostic}"
            ));
        }
        Ok(EvidenceWrite::Published | EvidenceWrite::AlreadyRecorded) => {}
        Err(error) => {
            return Err(RunTermination::Failed {
                message: error.to_string(),
            });
        }
    }
    let resume = if args.answers.is_empty() {
        None
    } else {
        Some(apply_resume_answers_once(&mut opened, &args.answers)?)
    };
    if !text.trim().is_empty()
        && let Driver::Interactive { seat, .. } = &driver
    {
        seat.busy();
    }

    mcp_registry
        .reconcile(refresh)
        .await
        .map_err(|error| RunTermination::Failed {
            message: error.to_string(),
        })?;
    let mcp_status = mcp_registry.status();
    let mcp_errors = mcp_status.mcp_errors();
    let admitted_tools = Some(mcp_registry.admitted_tools());

    let GenerationPreflight {
        source,
        selection,
        capabilities: main_capabilities,
        judge_enabled,
        proof_directory,
        proof_evidence,
    } = generation_preflight(
        GenerationPreflightInput {
            session_id,
            confinement: &confinement,
            effective: &effective,
            slug,
            cancel: &cancel,
            run_deadline,
            mcp_registry: Some(Arc::clone(&mcp_registry)),
            native_tools: &registry,
        },
        &mut status,
    )
    .await?;

    let opening = opened.state();
    drop(proof_directory);
    drop(proof_evidence);
    let mut active =
        ActiveSession::durable(opened, opening.clone(), EVIDENCE_CAP).map_err(|err| {
            RunTermination::Failed {
                message: format!("could not open this session's tool evidence: {err}"),
            }
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

    // An untrusted workspace resolves to `Mode::Default`; a trusted
    // interactive run may obtain its mode from the admitted config layers.
    let policy = match &driver {
        Driver::Once => {
            PolicyEngine::new(CanonPath::new(&root), mode, effective.permissions.clone())
        }
        Driver::Interactive { seat, .. } if seat.has_human() => PolicyEngine::with_human(
            CanonPath::new(&root),
            mode,
            effective.permissions.clone(),
            Arc::new(SeatApprover {
                seat: Arc::clone(seat),
            }),
        ),
        Driver::Interactive { .. } => {
            PolicyEngine::new(CanonPath::new(&root), mode, effective.permissions.clone())
        }
    };
    let fs = OsFs;
    let mut gate = budget::gate(&effective.ceilings, effective.clamped.money, currency);

    let mut config = TurnConfig {
        system,
        token_ceiling: effective.ceilings.tokens,
    };

    let turns = Turns {
        schema: compiled.as_ref(),
        retries: args.schema_retries,
        max_turns: args.max_turns,
        deadline: run_deadline,
    };

    // One turn's worth of loop, built from the state the last one left behind.
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
    // and a closure cannot name the lifetime of a `TurnLoop<'_>` it returns.
    macro_rules! build {
        ($active:expr, $state:expr, $cancel:expr, $ports:expr, $observation:expr) => {{
            let session = Session {
                id: $active.id(),
                state: $state,
                store: $active.store(),
                evidence: $active.run_evidence(),
                clock: &clock,
                sink,
                cancel: $cancel.clone(),
            };
            let http = Arc::new(ProviderHttpAdapter::new(Arc::clone(&$ports)))
                as Arc<dyn ego_ai::http::Http>;
            let runtime = GenerationRuntime {
                source: source.bind(http, $cancel.clone(), run_deadline),
                gate: &mut gate,
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
                    &policy,
                    effect_ports,
                    CanonPath::new(&cwd),
                )
                .map_err(|error| RunTermination::Failed {
                    message: format!("the admitted MCP tool catalogue is invalid: {error}"),
                })?,
                None => {
                    Tools::from_registry(&registry, &policy, effect_ports, CanonPath::new(&cwd))
                }
            };
            let loop_ = TurnLoop::new(session, runtime, tools);
            let loop_ = match $active.replayed() {
                Some(replayed) => loop_.with_ancestor_conversation(replayed.conversation().clone()),
                None => loop_,
            };
            let loop_ = match elicitor.as_ref() {
                Some(elicitor) => loop_.with_elicitor(Arc::clone(elicitor)),
                None => loop_,
            };
            let loop_ = if judge_enabled {
                loop_.with_judge(JudgeContext {
                    confinement: confinement.clone(),
                    exec_caps: $observation.capabilities(),
                })
            } else {
                loop_
            };
            loop_.with_execution_observation($observation)
        }};
    }

    if let Some(status) = status.as_ref()
        && status.human
    {
        status.seat.activate();
    }

    match driver {
        Driver::Once => {
            let input = match resume.as_ref() {
                Some(continuation) => HeadlessInput::Resumed(continuation.turn),
                None => HeadlessInput::Prompt(&text),
            };
            let (termination, state, result) = drive_headless_turn(
                &mut active,
                opening,
                &clock,
                sink,
                &cancel,
                Arc::clone(&source),
                Arc::clone(&evidence),
                invocation,
                &effective,
                config.clone(),
                &turns,
                &registry,
                admitted_tools.clone(),
                &policy,
                &cwd,
                &confinement,
                run_deadline,
                elicitor,
                judge_enabled,
                &args.attach,
                input,
                diagnostics,
            )
            .await?;
            active.replace_state(state.clone());
            Ok(Ended {
                termination,
                session: active.id(),
                state,
                result,
                mcp_errors: mcp_errors.clone(),
            })
        }
        Driver::Interactive { seat, between } => {
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
            let mut resumed = resume.as_ref().map(|continuation| continuation.turn);
            let mut pending_attachments = args.attach.clone();

            loop {
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
                    seat.set_compaction_availability(compaction_availability(pending_work));
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
                        repl::Line::Submitted(text) => Some(text),
                        repl::Line::Blank => continue,
                        // Ctrl+C at an idle prompt drops the line and nothing
                        // else. Someone who pressed it to stop a turn must not
                        // lose the session by pressing it once more.
                        repl::Line::Interrupted => continue,
                        repl::Line::Ended => break,
                        repl::Line::Refused(why) => {
                            diagnostics.note(why);
                            continue;
                        }
                        repl::Line::HostCommand(repl::HostCommand::Action(
                            repl::actions::ActionId::Sessions,
                        )) => {
                            match crate::catalogue::listing(&jsonl_store, &root, false) {
                                Ok(metas) => diagnostics
                                    .note(&crate::catalogue::table(&metas, false).join("\n")),
                                Err(error) => diagnostics.note(&format!(
                                    "the session catalogue could not be read: {error}"
                                )),
                            }
                            continue;
                        }
                        repl::Line::HostCommand(repl::HostCommand::Action(
                            repl::actions::ActionId::Usage,
                        )) => {
                            let metadata =
                                match crate::catalogue::listing(&jsonl_store, &root, false) {
                                    Ok(metadata) => metadata,
                                    Err(error) => {
                                        diagnostics.note(&format!(
                                            "the session catalogue could not be read: {error}"
                                        ));
                                        continue;
                                    }
                                };
                            let Some(meta) =
                                metadata.into_iter().find(|meta| meta.id == active.id())
                            else {
                                diagnostics.note(
                                    "the active session is absent from its durable catalogue",
                                );
                                continue;
                            };
                            match UsageReportService::new(&jsonl_store, &clock)
                                .report(std::slice::from_ref(&meta), UsagePeriod::All)
                                .await
                            {
                                Ok(report) => {
                                    let rendered = crate::usage::render_text(&report);
                                    diagnostics.note(rendered.trim_end());
                                }
                                Err(error) => diagnostics
                                    .note(&format!("the usage report could not be built: {error}")),
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
                            let target_opened = match session::open(&jsonl_store, &target, &clock) {
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
                                diagnostics.note("the terminal could not activate the new session");
                                continue;
                            }
                            active = target_owner;
                            state = active.state().clone();
                            last.session = active.id();
                            last.state = state.clone();
                            last.result = Value::String(String::new());
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
                            if let Some(status) = status.as_ref()
                                && status.human
                            {
                                match selection.snapshot() {
                                    Ok(selected) => status.seat.refresh_status(&state, selected),
                                    Err(error) => diagnostics.note(&format!(
                                        "the new session status selection could not be refreshed: {error}"
                                    )),
                                }
                            }
                            continue;
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
                                    diagnostics
                                        .note(&format!("the session cannot be forked: {error}"));
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
                            let target_opened = match session::open(&jsonl_store, &target, &clock) {
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
                            let publication = target_owner.publish_target(prepared.seed());
                            let diagnostic = publication.diagnostic().map(str::to_string);
                            if !active.apply_publication(target_owner, &publication) {
                                diagnostics.note(&format!(
                                    "the fork target was not published: {}",
                                    diagnostic.as_deref().unwrap_or("publication was refused")
                                ));
                                continue;
                            }
                            state = active.state().clone();
                            last.session = active.id();
                            last.state = state.clone();
                            last.result = Value::String(String::new());
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
                            if let Some(event) = project_public(prepared.seed().event()) {
                                sink.publish(&event, &state);
                            }
                            if !seat.activate_session(active.id()) {
                                last.termination = RunTermination::Failed {
                                    message: format!(
                                        "session {} was published but the terminal could not activate it",
                                        active.id()
                                    ),
                                };
                                break;
                            }
                            if let Some(status) = status.as_ref()
                                && status.human
                            {
                                match selection.snapshot() {
                                    Ok(selected) => status.seat.refresh_status(&state, selected),
                                    Err(error) => diagnostics.note(&format!(
                                        "the forked session status selection could not be refreshed: {error}"
                                    )),
                                }
                            }
                            continue;
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

                            let target_system = match instructions(
                                &effective,
                                &args,
                                &root,
                                &cwd,
                                diagnostics,
                            ) {
                                Ok(system) => system,
                                Err(error) => {
                                    diagnostics.note(&format!(
                                        "compaction target instructions could not be rebuilt: {error}"
                                    ));
                                    continue;
                                }
                            };
                            let target_registry = match tools(args.tools.as_ref()) {
                                Ok(registry) => registry,
                                Err(error) => {
                                    diagnostics.note(&format!(
                                        "compaction target tools could not be rebuilt: {error}"
                                    ));
                                    continue;
                                }
                            };
                            let target_tools: Vec<ToolDescriptor> = target_registry
                                .iter()
                                .map(|tool| tool.descriptor())
                                .collect();

                            let compaction_cancel = CancelToken::new();
                            let (ports, observation) = effect_scope(
                                &confinement,
                                active.evidence(),
                                &compaction_cancel,
                                run_deadline,
                            )
                            .await?;
                            let mut compaction =
                                build!(active, state, &compaction_cancel, ports, observation)
                                    .with_interactive_control(&mut control);
                            let prepared = compaction
                                .prepare_compaction(target_system.clone(), target_tools)
                                .await;
                            state = compaction.into_state();
                            active.replace_state(state.clone());
                            seat.refresh_queue(control.status());
                            let prepared = match prepared {
                                Ok(prepared) => prepared,
                                Err(error) => {
                                    diagnostics.note(&format!("compaction failed: {error}"));
                                    if let Some(status) = status.as_ref()
                                        && status.human
                                        && let Ok(selected) = selection.snapshot()
                                    {
                                        status.seat.refresh_status(&state, selected);
                                    }
                                    continue;
                                }
                            };

                            if !state.unacknowledged_prompts().is_empty()
                                || state.pending_question().is_some()
                                || !state.live_attempts.is_empty()
                            {
                                diagnostics.note(
                                    "compaction stopped before target creation because new source work became pending",
                                );
                                continue;
                            }

                            let target = SessionId::new();
                            let target_opened = match session::open(&jsonl_store, &target, &clock) {
                                Ok(opened) => Opened::Durable(Box::new(opened)),
                                Err(error) => {
                                    diagnostics.note(&format!(
                                        "compaction target could not be opened: {error}"
                                    ));
                                    continue;
                                }
                            };
                            let seed = prepared.target_seed(session::now(&clock));
                            let target_state = SessionState::replay([seed.event().clone()]);
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
                            let publication = target_owner.publish_target(&seed);
                            let diagnostic = publication.diagnostic().map(str::to_string);
                            if !active.apply_publication(target_owner, &publication) {
                                if let Some(diagnostic) = diagnostic {
                                    diagnostics.note(&format!(
                                        "compaction target was not published: {diagnostic}"
                                    ));
                                }
                                continue;
                            }

                            registry = target_registry;
                            config.system = target_system;
                            state = active.state().clone();
                            last.session = active.id();
                            last.state = state.clone();
                            last.result = Value::String(String::new());
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
                            if let Some(event) = project_public(seed.event()) {
                                sink.publish(&event, &state);
                            }
                            if !seat.activate_session(active.id()) {
                                last = Ended {
                                    termination: RunTermination::Failed {
                                        message: format!(
                                            "session {} was published but the terminal could not activate it",
                                            active.id()
                                        ),
                                    },
                                    session: active.id(),
                                    state: state.clone(),
                                    result: Value::String(String::new()),
                                    mcp_errors: mcp_errors.clone(),
                                };
                                break;
                            }
                            if let Some(status) = status.as_ref()
                                && status.human
                            {
                                match selection.snapshot() {
                                    Ok(selected) => status.seat.refresh_status(&state, selected),
                                    Err(error) => diagnostics.note(&format!(
                                        "the compacted session status selection could not be refreshed: {error}"
                                    )),
                                }
                            }
                            continue;
                        }
                        repl::Line::HostCommand(_) => continue,
                        repl::Line::ExternalBreak(_) => break,
                    }
                };

                // Per turn, for the reason `build` documents.
                seat.busy();
                let turn_cancel = CancelToken::new();
                let (ports, observation) =
                    effect_scope(&confinement, active.evidence(), &turn_cancel, run_deadline)
                        .await?;
                let images = if asked.is_some() && !pending_attachments.is_empty() {
                    let images = acquire_attachments(
                        &pending_attachments,
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
                let mut loop_ = build!(active, state, &turn_cancel, ports, observation)
                    .with_interactive_control(&mut control);
                let driven = match resumed_turn {
                    Some(turn) => {
                        let (termination, result) = turns
                            .drive_resumed(&mut loop_, turn, &turn_cancel, diagnostics)
                            .await;
                        Some((termination, result))
                    }
                    None => match asked {
                        Some(asked) => {
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
                        None => match loop_.run_queued_turn().await {
                            Ok(Some(outcome)) => {
                                let termination = loop_
                                    .state()
                                    .pending_question()
                                    .map(|pending| RunTermination::QuestionRequired {
                                        question: QuestionReport::from_request(
                                            pending.item_id(),
                                            pending.request(),
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
                                    Value::String(last_assistant_message(loop_.state())),
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
                                Value::String(last_assistant_message(loop_.state())),
                            )),
                        },
                    },
                };
                state = loop_.into_state();
                active.replace_state(state.clone());
                if let Some(status) = status.as_ref()
                    && status.human
                {
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

                let queue_abandoned = matches!(&termination, RunTermination::Failed { .. });
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
                    break;
                }
                if queue_abandoned {
                    break;
                }
            }

            Ok(last)
        }
    }
}

/// What drives the turns once the session is open.
///
/// The setup above is identical either way — the same resolution, the same
/// session, the same policy — so it is the tail that branches rather than the
/// function that is duplicated.
enum Driver<'a> {
    /// One prompt from argv or stdin, and the run is over.
    Once,
    /// Lines from a person, until they stop.
    Interactive {
        seat: Arc<dyn InteractiveSeat>,
        /// Run after each turn: the screen holds the last block back until it
        /// knows no more is coming, and between turns is when it knows.
        between: &'a (dyn Fn() + Send + Sync),
    },
}

struct RunHost<'a> {
    driver: Driver<'a>,
    elicitor: Option<Arc<dyn Elicitor>>,
    status: Option<InteractiveStatus>,
    diagnostics: &'a dyn Diagnostics,
    mcp_registry: Arc<McpRegistryController>,
    evidence: Arc<EvidenceAdmissionService>,
}

struct InteractiveStatus {
    seat: Arc<repl::Seat>,
    width: usize,
    human: bool,
    native: bool,
    /// The run owns the same picker instance installed into the terminal seat.
    picker: Option<Arc<Mutex<ModelPicker>>>,
}

fn run_trust(args: &Args, driver: &Driver<'_>, root: &Path) -> Trust {
    match driver {
        Driver::Once => Trust::Untrusted,
        Driver::Interactive { seat, .. } if !args.bare && seat.has_human() => seat.trust(root),
        Driver::Interactive { .. } => Trust::Untrusted,
    }
}

/// The seat, as the run loop needs it.
///
/// A trait so a test can drive the whole interactive path from a script of
/// lines, with no terminal and no editor — the real [`repl::Seat`] needs a tty
/// and would make every one of those tests unrunnable in CI.
pub trait Reads {
    /// Read one line, blocking until there is one.
    fn read(&self) -> repl::Line;
}

/// The two explicit decisions only a human-driven run may make.
pub trait InteractiveSeat: Reads + std::fmt::Debug + Send + Sync {
    fn has_human(&self) -> bool;
    fn busy(&self) {}
    fn idle(&self) {}
    /// Hand the session-long core control endpoint to the turn coordinator.
    fn take_control(&self) -> InteractiveControl {
        let (_ingress, control) = interactive_control();
        control
    }
    /// Publish the coordinator's authoritative durable queue projection.
    fn refresh_queue(&self, _queue: QueueStatus) {}
    /// Publish the coordinator-owned compaction guard to the interactive surface.
    fn set_compaction_availability(&self, _availability: repl::actions::Availability) {}
    /// Confirm a source-owned clean-session transition.
    fn confirm_compaction(&self, _source: SessionId) -> bool {
        false
    }
    /// Switch the terminal and its draft namespace after publication.
    fn activate_session(&self, _target: SessionId) -> bool {
        false
    }
    fn trust(&self, workspace: &Path) -> Trust;
    fn permission(&self, request: &PermissionRequest) -> PermissionDecision;
    /// Await a modal decision without blocking the core's intent polling.
    fn permission_future<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> futures::future::BoxFuture<'a, PermissionDecision> {
        Box::pin(futures::future::ready(self.permission(request)))
    }
}

#[derive(Debug)]
struct SeatApprover {
    seat: Arc<dyn InteractiveSeat>,
}

impl HumanApprover for SeatApprover {
    fn decide<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> futures::future::BoxFuture<'a, PermissionDecision> {
        self.seat.permission_future(request)
    }
}

/// The run-level limits, and the loop that applies them between turns.
///
/// Between turns is where they belong: `max_turns` fires *between* two
/// completed turns and is not honestly a cause of the preceding one, and a
/// token budget cannot interrupt a generation that is already paid for.
struct Turns<'a> {
    schema: Option<&'a schema::Schema>,
    retries: u32,
    max_turns: Option<u32>,
    deadline: Option<std::time::Instant>,
}

impl Turns<'_> {
    fn deadline_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    }

    async fn drive_with_images(
        &self,
        loop_: &mut TurnLoop<'_>,
        first: &str,
        images: Vec<ImageRef>,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, first, images, None, cancel, diagnostics)
            .await
    }

    async fn drive_resumed(
        &self,
        loop_: &mut TurnLoop<'_>,
        turn: ego_core::event::id::TurnId,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, "", Vec::new(), Some(turn), cancel, diagnostics)
            .await
    }

    async fn drive_from(
        &self,
        loop_: &mut TurnLoop<'_>,
        first: &str,
        mut images: Vec<ImageRef>,
        mut resumed: Option<ego_core::event::id::TurnId>,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        let mut prompt = first.to_string();
        let mut taken: u32 = 0;
        let mut left = self.retries;

        loop {
            if self.max_turns.is_some_and(|max| taken >= max) {
                return (RunTermination::MaxTurns, self.answer(loop_.state()));
            }
            let outcome = match resumed.take() {
                Some(turn) => self.one_resumed(loop_, turn, cancel).await,
                None => {
                    self.one(loop_, &prompt, std::mem::take(&mut images), cancel)
                        .await
                }
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(termination) => return (termination, self.answer(loop_.state())),
            };
            taken += 1;

            if let Some(pending) = loop_.state().pending_question() {
                return (
                    RunTermination::QuestionRequired {
                        question: QuestionReport::from_request(
                            pending.item_id(),
                            pending.request(),
                        ),
                    },
                    self.answer(loop_.state()),
                );
            }

            if let Some(termination) =
                RunTermination::after_turn(outcome.exit_reason, outcome.denied)
            {
                return (termination, self.answer(loop_.state()));
            }

            // Durable steering owns the next provider request. A schema
            // repair is another user turn, so it must wait until the outer
            // interactive coordinator has replayed the accepted FIFO through
            // a fresh TurnLoop and cancellation token.
            if !loop_.state().unacknowledged_prompts().is_empty() {
                return (RunTermination::Done, self.answer(loop_.state()));
            }

            let answer = last_assistant_message(loop_.state());
            let Some(schema) = self.schema else {
                return (RunTermination::Done, Value::String(answer));
            };
            match schema.check(&answer) {
                Ok(document) => return (RunTermination::Done, document),
                Err(invalid) => {
                    diagnostics.note(&format!("the answer did not validate: {invalid}"));
                    if left == 0 {
                        return (RunTermination::TaskFailed, Value::String(answer));
                    }
                    left -= 1;
                    prompt = schema::repair_prompt(&invalid);
                }
            }
        }
    }

    /// One turn, watched by the two things only a process can see: a signal,
    /// and a deadline.
    ///
    /// The cancellation is cooperative first and bounded second. The token is
    /// set so the loop unwinds cleanly — settling its attempt, writing
    /// `TurnCompleted` — and only if it has not finished within [`GRACE`] does
    /// the run stop waiting and report from the cause. Dropping the future
    /// immediately would abandon an unsettled reservation; waiting forever
    /// would make `--timeout` advisory.
    async fn one(
        &self,
        loop_: &mut TurnLoop<'_>,
        prompt: &str,
        images: Vec<ImageRef>,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        let (outcome, stopped) = {
            let mut running = Box::pin(loop_.run_with_images(prompt, images));
            let mut stopped = false;
            loop {
                if stopped {
                    tokio::select! {
                        done = &mut running => break (Some(done), stopped),
                        () = tokio::time::sleep(GRACE) => break (None, stopped),
                    }
                }
                tokio::select! {
                    biased;
                    cause = stop_signal(self.deadline) => {
                        cancel.cancel(cause);
                        stopped = true;
                    }
                    done = &mut running => {
                        if self.deadline_expired() {
                            cancel.cancel(CancelCause::Timeout);
                            stopped = true;
                        }
                        break (Some(done), stopped);
                    }
                }
            }
        };

        if stopped {
            return Err(cancelled_termination(cancel));
        }
        match outcome {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(abandoned)) => Err(RunTermination::Failed {
                message: format!("the session log could not be written: {}", abandoned.cause),
            }),
            // The turn did not unwind in time. The cause is still the honest
            // account of why the run ended, and it is the same mapping a
            // cancelled turn would have produced.
            None => Err(match cancel.cause() {
                Some(CancelCause::Timeout) => RunTermination::TimedOut,
                Some(CancelCause::User) => RunTermination::Signalled(Signal::Int),
                _ => RunTermination::Signalled(Signal::Term),
            }),
        }
    }

    async fn one_resumed(
        &self,
        loop_: &mut TurnLoop<'_>,
        turn: ego_core::event::id::TurnId,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        let (outcome, stopped) = {
            let mut running = Box::pin(loop_.resume_turn(turn));
            let mut stopped = false;
            loop {
                if stopped {
                    tokio::select! {
                        done = &mut running => break (Some(done), stopped),
                        () = tokio::time::sleep(GRACE) => break (None, stopped),
                    }
                }
                tokio::select! {
                    biased;
                    cause = stop_signal(self.deadline) => {
                        cancel.cancel(cause);
                        stopped = true;
                    }
                    done = &mut running => {
                        if self.deadline_expired() {
                            cancel.cancel(CancelCause::Timeout);
                            stopped = true;
                        }
                        break (Some(done), stopped);
                    }
                }
            }
        };
        if stopped {
            return Err(cancelled_termination(cancel));
        }
        match outcome {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(abandoned)) => Err(RunTermination::Failed {
                message: format!("the session log could not be written: {}", abandoned.cause),
            }),
            None => Err(cancelled_termination(cancel)),
        }
    }

    /// The last assistant message, as `CLI.md` §4.1's `result`.
    fn answer(&self, state: &SessionState) -> Value {
        Value::String(last_assistant_message(state))
    }
}

/// Whichever of a signal or the deadline arrives first, and who it was.
///
/// A `SIGINT` and an expired `--timeout` are different exits (130 and 124), so
/// this returns the cause rather than the fact — and `CancelToken` keeps the
/// first one, so a deadline that expires a moment after the user pressed
/// Ctrl-C does not rewrite what happened.
pub async fn stop_signal(deadline: Option<std::time::Instant>) -> CancelCause {
    let deadline = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
            // Nothing to wait for; never resolves, so `select!` ignores it.
            None => std::future::pending().await,
        }
    };
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a process may always listen for SIGTERM");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => CancelCause::User,
        _ = term.recv() => CancelCause::Host,
        () = deadline => CancelCause::Timeout,
    }
}

/// The text of the last assistant message in the session.
///
/// Read from `SessionState` rather than accumulated while streaming: the state
/// is the fold of the durable log, so what is reported is what was written
/// down. An empty string when the run produced no assistant message — §4.1's
/// `result` is a string, and a run that said nothing said the empty thing.
fn last_assistant_message(state: &SessionState) -> String {
    state
        .items
        .values()
        .filter(|item| item.kind == ItemKind::Assistant)
        .max_by_key(|item| item.seq)
        .map(|item| item.text.clone())
        .unwrap_or_default()
}

/// `ego ls`: the catalogue, printed (`CLI.md` §4.6).
///
/// A `RunTermination` like everything else, so one exit-code table answers for
/// the whole binary: a store that cannot be listed is `Failed` (exit 1) and a
/// store root that cannot be located is a `UsageError` (exit 2), exactly as
/// they are for a run. There is no session and no turn, so nothing here can
/// produce the endings that are about one.
pub fn ls(args: &crate::cli::LsArgs, out: &Stdout) -> RunTermination {
    match listed(args, out) {
        Ok(()) => RunTermination::Done,
        Err(termination) => termination,
    }
}

/// `ego fork ID`: publish one clean target at the source's latest idle event.
pub fn fork(args: &ForkArgs, out: &Stdout) -> Ended {
    match forked(args, out) {
        Ok(ended) => ended,
        Err(termination) => Ended::refused(termination),
    }
}

fn forked(args: &ForkArgs, out: &Stdout) -> Result<Ended, RunTermination> {
    let usage = |error: UsageError| RunTermination::UsageError {
        message: error.to_string(),
    };
    let root = workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = rooted_path(
        &root,
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
    );
    let store = session::jsonl(store_root, root.clone());
    let source = crate::catalogue::resolve(&store, &root, Some(args.source)).map_err(usage)?;
    let clock = SystemClock;
    let opened =
        session::open(&store, &source, &clock).map_err(|error| RunTermination::Failed {
            message: format!("the fork source could not be opened: {error}"),
        })?;
    let opened = Opened::Durable(Box::new(opened));
    if let Some(note_text) = opened.repair_note() {
        out.note(&note_text);
    }
    let Opened::Durable(opened) = opened;
    fork_opened(args, out, &store, source, *opened, &clock)
}

fn fork_opened(
    args: &ForkArgs,
    out: &Stdout,
    store: &JsonlStore,
    source: SessionId,
    opened: ego_tools::store::OpenSession<'_>,
    clock: &SystemClock,
) -> Result<Ended, RunTermination> {
    let source_state = opened.state.clone();
    let prepared = ego_core::session::PreparedFork::new(source, &source_state, session::now(clock))
        .map_err(|error| RunTermination::Failed {
            message: format!("the fork source is not idle: {error}"),
        })?;
    let replayed = ego_core::session::ReplayContext::new(&opened.writer, &opened.writer)
        .replay_lineage(prepared.seed().lineage())
        .map_err(|error| RunTermination::Failed {
            message: format!("the fork source context could not be replayed: {error}"),
        })?;

    let target = SessionId::new();
    let target_opened =
        session::open(store, &target, clock).map_err(|error| RunTermination::Failed {
            message: format!("the fork target could not be opened: {error}"),
        })?;
    let publication = {
        use ego_core::session::TargetStore;
        target_opened.writer.publish_target(prepared.seed())
    };
    let diagnostic = publication.diagnostic().map(str::to_string);
    if !publication.switch_target() {
        return Err(RunTermination::Failed {
            message: format!(
                "the fork target was not published: {}",
                diagnostic.as_deref().unwrap_or("publication was refused")
            ),
        });
    }
    drop(replayed);
    if let Some(diagnostic) = diagnostic {
        out.note(&format!(
            "fork target {} is authoritative with a durability warning: {diagnostic}",
            target
        ));
    }

    let target_state = SessionState::replay([prepared.seed().event().clone()]);
    let mut ended = Ended {
        termination: RunTermination::Done,
        session: target,
        state: target_state,
        result: Value::String(target.to_string()),
        mcp_errors: Vec::new(),
    };
    let rendered = match args.output {
        Output::Text => {
            out.line(&target.to_string());
            Ok(())
        }
        Output::Json | Output::Jsonl => render(out, &report(&ended), "the fork result"),
    };
    if let Err(termination) = rendered {
        ended.termination = termination;
    }
    Ok(ended)
}

/// [`ls`], with the endings as an error so each step can be one line.
///
/// The `Ok(())` case is `RunTermination::Done` and nothing else, so there is
/// no ending decided here that [`ls`] does not name.
fn listed(args: &crate::cli::LsArgs, out: &Stdout) -> Result<(), RunTermination> {
    let usage = |err: UsageError| RunTermination::UsageError {
        message: err.to_string(),
    };

    let root = workspace(args.directory.as_ref()).map_err(usage)?;
    let store_root = rooted_path(
        &root,
        session::store_root(args.session_dir.as_ref()).map_err(usage)?,
    );
    // The workspace is handed over for the same reason `run` hands it over —
    // a store writes it into the header of any session it creates — even
    // though listing creates none. One constructor, one meaning.
    let store = session::jsonl(store_root, root.clone());

    let metas = crate::catalogue::listing(&store, &root, args.all).map_err(|err| {
        RunTermination::Failed {
            message: format!("the session store could not be listed: {err}"),
        }
    })?;

    match args.output {
        // §4.6's single object. Rendered even when there is nothing to list:
        // a script that gets no output cannot tell an empty store from a
        // command that failed to run.
        Output::Json => render(out, &crate::catalogue::Listing::of(&metas), "the listing")?,
        // One session per line, which is what §4.2's spelling means for a
        // list: the unit of a catalogue is a session, as the unit of a run is
        // an event.
        Output::Jsonl => {
            for meta in &metas {
                render(out, &crate::catalogue::Row::of(meta), "a session")?;
            }
        }
        Output::Text => tabulate(out, &metas, &root, args.all),
    }
    Ok(())
}

/// The catalogue as a table, and the sentence an empty one owes.
///
/// The header row is printed even for an empty store, so the person who typed
/// `ego ls` sees that the command ran. The empty case says so on stderr rather
/// than on stdout, which stays the listing — which is also why this cannot
/// fail: nothing is serialised, so there is no rendering to refuse.
fn tabulate(out: &Stdout, metas: &[SessionMeta], root: &Path, all: bool) {
    for line in crate::catalogue::table(metas, all) {
        out.line(&line);
    }
    if metas.is_empty() {
        note(&if all {
            "no sessions in this store".to_string()
        } else {
            format!(
                "no session belongs to {}; `ego ls --all` lists every one",
                root.display()
            )
        });
    }
}

/// One JSON value on one line, or the ending a value that will not serialise
/// produces.
///
/// `what` names the thing rather than the error: a `Listing` and a `Row` fail
/// for the same reason and the operator needs to know which of the two the
/// command was in the middle of.
fn render<T: serde::Serialize>(out: &Stdout, value: &T, what: &str) -> Result<(), RunTermination> {
    match serde_json::to_string(value) {
        Ok(line) => {
            out.line(&line);
            Ok(())
        }
        Err(err) => Err(RunTermination::Failed {
            message: format!("{what} could not be rendered: {err}"),
        }),
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
        RunTermination::UsageError { message } | RunTermination::Provider { message } => {
            message.clone()
        }
        RunTermination::Failed { message } => message.clone(),
        RunTermination::QuestionRequired { question } => format!(
            "stopped: human input required — answer with `ego resume --answer {}=JSON`",
            question.id
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

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
        assert_eq!(
            run_trust(&interactive_args(&[]), &interactive, Path::new("/ws")),
            Trust::Trusted
        );
        assert_eq!(seat.asked.load(Ordering::SeqCst), 1);

        assert_eq!(
            run_trust(
                &interactive_args(&["--bare"]),
                &interactive,
                Path::new("/ws")
            ),
            Trust::Untrusted
        );
        assert_eq!(
            seat.asked.load(Ordering::SeqCst),
            1,
            "--bare must skip the seat"
        );

        assert_eq!(
            run_trust(&interactive_args(&[]), &Driver::Once, Path::new("/ws")),
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
}
