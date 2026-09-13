//! Own interactive process entry, terminal announcements, and model switching.

use super::*;

/// The currency a session settles in.
///
/// One constant and not a flag, because `--currency` is not in `CLI.md` §3 and
/// a currency nobody can state is worse than a stated one: every quote this
/// build can produce is either zero (a local daemon) or supplied alongside its
/// own currency.
pub(super) const SETTLEMENT: &str = "USD";

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

pub(super) fn model_switch_occurrence(
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

pub(super) enum ResumePlan {
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

pub(super) fn plan_resume_answer(
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

pub(super) fn apply_model_resume_once(
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
pub(super) const GRACE: Duration = Duration::from_secs(5);

/// Complete stdout or stderr beyond this per-stream bound fails the tool and
/// terminates its process tree. Preview memory is bounded separately by
/// [`ego_tools::exec::ExecLimits`].
pub(super) const EVIDENCE_CAP: u64 = 64 * 1024 * 1024;

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
    pub(super) fn refused(termination: RunTermination) -> Self {
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

pub(super) async fn run_process(
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

pub(super) async fn interactive_process(
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

pub(super) struct InteractiveServices {
    evidence: Arc<EvidenceAdmissionService>,
}

pub(super) async fn interactive_through(
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
    let between = || {
        text_sink.finish();
        // A terminal output queue is asynchronous. Its empty sentinel is the
        // barrier that commits the final answer row before `seat.idle()` may
        // repaint a fresh composer for the next turn.
        output.finish();
    };
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
pub(super) struct Announce<'a> {
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
pub(super) const NOTIFY_AFTER: Duration = Duration::from_secs(60);

impl<'a> Announce<'a> {
    pub(super) fn new(out: &'a dyn TextOutput, capabilities: Capabilities, quiet: bool) -> Self {
        Announce {
            out,
            osc: Osc::new(capabilities),
            quiet,
            started: std::time::Instant::now(),
        }
    }

    pub(super) fn opening(&self, prompt: Option<&str>) {
        self.emit(self.preamble(prompt));
    }

    pub(super) fn closing(&self, termination: &RunTermination) {
        self.emit(self.postamble(termination, self.started.elapsed()));
    }

    /// Write a sequence, or nothing at all.
    ///
    /// The empty check is not an optimisation: [`Stdout::write`] flushes, and a
    /// run in `--output json` would otherwise flush for the privilege of
    /// writing zero bytes.
    pub(super) fn emit(&self, sequence: String) {
        if !sequence.is_empty() {
            self.out.write(&sequence);
        }
    }

    /// One string rather than four writes, so a pipe that breaks mid-preamble
    /// cannot leave half of an escape sequence in the stream — and so the whole
    /// announcement is a value a test can read.
    pub(super) fn preamble(&self, prompt: Option<&str>) -> String {
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

    pub(super) fn postamble(&self, termination: &RunTermination, elapsed: Duration) -> String {
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
pub(super) fn title(prompt: Option<&str>) -> String {
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
pub(super) fn duration(elapsed: Duration) -> String {
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
pub(super) async fn run_with_registry<S: EventSink>(
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
