//! What drives the turns once the session is open.
//!
//! `execute` sets a session up; this is what happens to it afterwards, in
//! either posture: one prompt from argv or stdin, or lines from a person until
//! they stop. The setup is identical either way, so it is the tail that
//! branches rather than the whole function that is duplicated.
//!
//! Glob-imported from the parent for the same reason as `executor`: this is
//! `execute`'s own tail, split out for navigability, and it reaches back into
//! the session the caller has already opened.
use super::*;

/// What drives the turns once the session is open.
///
/// The setup above is identical either way — the same resolution, the same
/// session, the same policy — so it is the tail that branches rather than the
/// function that is duplicated.
pub(super) enum Driver<'a> {
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

pub(super) struct RunHost<'a> {
    pub(super) driver: Driver<'a>,
    pub(super) elicitor: Option<Arc<dyn Elicitor>>,
    pub(super) status: Option<InteractiveStatus>,
    pub(super) diagnostics: &'a dyn Diagnostics,
    pub(super) text_output: Option<&'a dyn TextOutput>,
    pub(super) evidence: Arc<EvidenceAdmissionService>,
}

pub(super) struct InteractiveStatus {
    pub(super) seat: Arc<repl::Seat>,
    pub(super) width: usize,
    pub(super) human: bool,
    pub(super) native: bool,
    /// The run owns the same picker instance installed into the terminal seat.
    pub(super) picker: Option<Arc<Mutex<ModelPicker>>>,
    pub(super) model_config_store: Option<Arc<dyn UserConfigStore>>,
}

/// `home` is where a remembered answer lives; `None` is a process with no ego
/// home at all, which asks every launch and records nothing.
///
/// `--bare` is already an explicit refusal, so it reaches neither the store
/// nor the seat — it does not read a remembered answer and does not write one.
pub(super) fn run_trust(
    args: &Args,
    driver: &Driver<'_>,
    root: &Path,
    home: Option<&Path>,
    diagnostics: &dyn Diagnostics,
) -> Trust {
    let selected = args
        .instructions
        .as_ref()
        .and_then(|path| explicit(path, root).ok());
    if args.bare {
        return Trust::Untrusted;
    }
    crate::trust::resolve_selected(
        root,
        home,
        diagnostics,
        args.trust_workspace,
        selected.as_deref(),
        |context| match driver {
            Driver::Interactive { seat, .. } if seat.has_human() => {
                let question_context = if args.quiet {
                    context.to_string()
                } else {
                    format!(
                        "ego {} · {}\n{context}",
                        env!("CARGO_PKG_VERSION"),
                        args.model.as_deref().unwrap_or("model pending")
                    )
                };
                Some(seat.trust(root, &question_context))
            }
            _ => None,
        },
    )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteboardKind {
    Image,
    Text,
    Empty,
}

/// The two explicit decisions only a human-driven run may make.
pub trait InteractiveSeat: Reads + std::fmt::Debug + Send + Sync {
    fn has_human(&self) -> bool;
    fn width(&self) -> usize {
        80
    }
    fn action_help(&self) -> String {
        crate::repl::render_action_help(
            &crate::repl::actions::ActionRegistry::final_surface(),
            self.width().saturating_sub(4),
        )
    }
    fn key_hints(&self) -> String {
        crate::repl::render_key_hints(
            &crate::repl::actions::ActionRegistry::final_surface(),
            self.width(),
        )
    }
    /// Collect a persistent model choice before the first run is materialized.
    fn read_startup_model(
        &self,
        picker: Arc<Mutex<ModelPicker>>,
        notice: Option<String>,
    ) -> crate::model_picker::PickerOutcome;
    /// Read-only host requests can be answered without waiting for a model turn.
    fn next_inspection(&self) -> futures::future::BoxFuture<'_, repl::HostCommand> {
        Box::pin(futures::future::pending())
    }
    fn busy(&self) {}
    fn idle(&self) {}
    /// How long the current or last turn waited on a modal.
    fn modal_wait(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    fn set_image_chips(&self, _chips: Vec<String>) {}
    fn set_notice(&self, _notice: String) {}
    fn pasteboard_kind(&self) -> Result<PasteboardKind, String> {
        Err("OS pasteboard is unavailable".to_owned())
    }
    fn pasteboard_image(&self, _max_bytes: u64) -> Result<Vec<u8>, String> {
        Err("OS pasteboard image is unavailable".to_owned())
    }
    fn pasteboard_text(&self) -> Result<String, String> {
        Err("OS pasteboard text is unavailable".to_owned())
    }
    fn insert_paste_text(&self, _text: String) {}
    /// Hand the session-long core control endpoint to the turn coordinator.
    fn take_control(&self) -> InteractiveControl {
        let (_ingress, control) = interactive_control();
        control
    }
    /// Return terminal ingress ownership before re-entering rooted admission.
    fn restore_control(&self, _control: InteractiveControl) {}
    /// Publish the coordinator's authoritative durable queue projection.
    fn refresh_queue(&self, _queue: QueueStatus) {}
    /// Publish the coordinator-owned compaction guard to the interactive surface.
    fn set_compaction_availability(&self, _availability: repl::actions::Availability) {}
    /// Confirm a source-owned clean-session transition.
    fn confirm_compaction(&self, _source: SessionId) -> bool {
        false
    }
    /// Select an existing session through the terminal owner.
    fn pick_session(
        &self,
        _sessions: Vec<ego_core::store::session_store::SessionMeta>,
    ) -> Option<SessionId> {
        None
    }
    /// Switch the terminal and its draft namespace after publication.
    fn activate_session(&self, _target: SessionId) -> bool {
        false
    }
    fn trust(&self, workspace: &Path, context: &str) -> Trust;
    /// Acknowledge the Windows host's lack of confinement for this workspace.
    /// Test and non-human seats deny unless they explicitly own this authority.
    fn acknowledge_windows_unconfined(&self, _workspace: &Path) -> bool {
        false
    }
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
pub(super) struct SeatApprover {
    pub(super) seat: Arc<dyn InteractiveSeat>,
}

impl HumanApprover for SeatApprover {
    fn decide<'a>(
        &'a self,
        _item: ItemId,
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
pub(super) struct Turns<'a> {
    pub(super) schema: Option<&'a schema::Schema>,
    pub(super) retries: u32,
    pub(super) max_turns: Option<u32>,
    pub(super) deadline: Option<std::time::Instant>,
}

enum TurnStart {
    Checkpoint,
    Prompt {
        text: String,
        images: Vec<ImageRef>,
    },
    Resumed(ego_core::event::id::TurnId),
    Presented(ItemId),
    Answered {
        question: ItemId,
        answer: QuestionAnswer,
    },
    Skill(AdmittedSkillOperation),
    McpContext(repl::actions::McpContextCommand),
}

impl Turns<'_> {
    fn deadline_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| std::time::Instant::now() >= deadline)
    }

    pub(super) async fn drive_with_images<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        first: &str,
        images: Vec<ImageRef>,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(
            loop_,
            TurnStart::Prompt {
                text: first.to_owned(),
                images,
            },
            cancel,
            diagnostics,
        )
        .await
    }

    pub(super) async fn drive_admitted_skill<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        operation: AdmittedSkillOperation,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Skill(operation), cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_mcp_context<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        command: repl::actions::McpContextCommand,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::McpContext(command), cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_checkpoint<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Checkpoint, cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_resumed<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        turn: ego_core::event::id::TurnId,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Resumed(turn), cancel, diagnostics)
            .await
    }

    pub(super) async fn drive_answered<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        question: ItemId,
        answer: QuestionAnswer,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(
            loop_,
            TurnStart::Answered { question, answer },
            cancel,
            diagnostics,
        )
        .await
    }

    pub(super) async fn drive_presented<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        question: ItemId,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        self.drive_from(loop_, TurnStart::Presented(question), cancel, diagnostics)
            .await
    }

    async fn drive_from<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        mut next: TurnStart,
        cancel: &CancelToken,
        diagnostics: &dyn Diagnostics,
    ) -> (RunTermination, Value) {
        let mut taken: u32 = 0;
        let mut left = self.retries;

        loop {
            if self.max_turns.is_some_and(|max| taken >= max) {
                return (RunTermination::MaxTurns, self.answer(&loop_.state()));
            }
            let before_turns = loop_.state().turns.len();
            loop_.set_turn_allowance(self.max_turns.map(|max| max.saturating_sub(taken)));
            let outcome = match next {
                TurnStart::Checkpoint => {
                    self.one_running(Box::pin(loop_.continue_checkpoint()), cancel)
                        .await
                }
                TurnStart::Prompt { text, images } => self.one(loop_, &text, images, cancel).await,
                TurnStart::Resumed(turn) => self.one_resumed(loop_, turn, cancel).await,
                TurnStart::Presented(question) => {
                    self.one_running(Box::pin(loop_.present_foreign_question(question)), cancel)
                        .await
                }
                TurnStart::Answered { question, answer } => {
                    self.one_answered(loop_, question, answer, cancel).await
                }
                TurnStart::Skill(operation) => self.one_skill(loop_, operation, cancel).await,
                TurnStart::McpContext(command) => {
                    self.one_running(
                        Box::pin(loop_.run_admitted_context(
                            command.catalogue,
                            command.name,
                            command.parameters,
                        )),
                        cancel,
                    )
                    .await
                }
            };
            let outcome = match outcome {
                Ok(outcome) => outcome,
                Err(termination) => return (termination, self.answer(&loop_.state())),
            };
            taken = taken.saturating_add(
                u32::try_from(loop_.state().turns.len().saturating_sub(before_turns))
                    .unwrap_or(u32::MAX)
                    .max(1),
            );
            if loop_
                .state()
                .goal
                .as_ref()
                .and_then(|goal| goal.check_stop.as_deref())
                == Some("run max-turns reached")
            {
                return (RunTermination::MaxTurns, self.answer(&loop_.state()));
            }

            if let Some(pending) = loop_.state().pending_question() {
                return (
                    RunTermination::QuestionRequired {
                        question: Box::new(QuestionReport::from_request(
                            pending.item_id(),
                            pending.target(),
                            pending.request(),
                        )),
                    },
                    self.answer(&loop_.state()),
                );
            }

            if let Some(skill) = outcome.skill.as_ref() {
                return match loop_.materialize_skill_outcome(skill) {
                    Ok((true, text)) => (RunTermination::Done, Value::String(text)),
                    Ok((false, diagnostic)) => {
                        diagnostics.note(&diagnostic);
                        (RunTermination::TaskFailed, Value::String(diagnostic))
                    }
                    Err(error) => (
                        RunTermination::Failed {
                            message: error.to_string(),
                        },
                        Value::String(error.to_string()),
                    ),
                };
            }

            if let Some(termination) = RunTermination::after_outcome(&outcome) {
                return (termination, self.answer(&loop_.state()));
            }

            // Durable steering owns the next provider request. A schema
            // repair is another user turn, so it must wait until the outer
            // interactive coordinator has replayed the accepted FIFO through
            // a fresh TurnLoop and cancellation token.
            if !loop_.state().unacknowledged_prompts().is_empty() {
                return (RunTermination::Done, self.answer(&loop_.state()));
            }

            let answer = last_assistant_message(&loop_.state());
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
                    next = TurnStart::Prompt {
                        text: schema::repair_prompt(&invalid),
                        images: Vec::new(),
                    };
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
    async fn one<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        prompt: &str,
        images: Vec<ImageRef>,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        self.one_running(Box::pin(loop_.run_with_images(prompt, images)), cancel)
            .await
    }

    async fn one_resumed<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        turn: ego_core::event::id::TurnId,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        self.one_running(Box::pin(loop_.resume_turn(turn)), cancel)
            .await
    }

    async fn one_answered<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        question: ItemId,
        answer: QuestionAnswer,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        self.one_running(Box::pin(loop_.resume_question(question, answer)), cancel)
            .await
    }

    async fn one_skill<'session: 'turn, 'turn>(
        &self,
        loop_: &mut TurnLoop<'session, 'turn>,
        operation: AdmittedSkillOperation,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination> {
        self.one_running(Box::pin(loop_.run_admitted_skill(operation)), cancel)
            .await
    }

    async fn one_running<F>(
        &self,
        running: F,
        cancel: &CancelToken,
    ) -> Result<TurnOutcome, RunTermination>
    where
        F: Future<Output = Result<TurnOutcome, TurnAbandoned>>,
    {
        let (outcome, stopped) = {
            let mut running = Box::pin(running);
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
    tokio::select! {
        cause = process_signal() => cause,
        () = deadline => CancelCause::Timeout,
    }
}

#[cfg(unix)]
async fn process_signal() -> CancelCause {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a process may always listen for SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => CancelCause::User,
        _ = term.recv() => CancelCause::Host,
    }
}

#[cfg(windows)]
async fn process_signal() -> CancelCause {
    tokio::signal::ctrl_c()
        .await
        .expect("a process may always listen for Ctrl-C");
    CancelCause::User
}

/// The text of the last assistant message in the session.
///
/// Read from `SessionState` rather than accumulated while streaming: the state
/// is the fold of the durable log, so what is reported is what was written
/// down. An empty string when the run produced no assistant message — §4.1's
/// `result` is a string, and a run that said nothing said the empty thing.
pub(super) fn last_assistant_message(state: &SessionState) -> String {
    state
        .items
        .values()
        .filter(|item| item.kind == ItemKind::Assistant)
        .max_by_key(|item| item.seq)
        .map(|item| item.text.clone())
        .unwrap_or_default()
}
