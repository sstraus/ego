//! The seat: one lifetime owner for the interactive terminal.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam::channel::{Receiver, SendError, Sender, bounded};
use crossterm::event::Event;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use reedline::{
    ColumnarMenu, Completer, CompletionResult, EditCommand, EditMode, Emacs, ExternalPrinter,
    KeyCode, KeyModifiers, MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, Reedline,
    ReedlineEvent, ReedlineMenu, ReedlineRawEvent, Span, Suggestion, default_emacs_keybindings,
};

use super::actions::{
    ActionDispatch, ActionId, ActionRegistry, ActionSink, Availability, CompositeActionSink,
    CompositeActionSinkError, Dispatch, InputOrigin, LocalAction, LocalActionOutcome,
    SharedActionSink, SkillSurfaceAction,
};
use super::keys::{Intercept, intercept};
use super::policy_controls::{
    ModalInput, PermissionInput, PolicyControlController, PolicyControlRequest,
};
use super::selector::{Selector, SelectorKey, SelectorKind, SelectorOption, SelectorValue};
use super::status::{
    GitFacts, GitFactsSource, SeatFacts, SelectionFacts, StatusLine, StatusSegmentName,
    SurfaceSnapshot,
};
use super::surface::InteractivePainter;
use super::{BUSY_SUBMIT_COMMAND, DRAFT_COMMAND, HostCommand, INTERRUPT_COMMAND, Line, answer};
use crate::cli::{managed_rule, managed_rule_effect, managed_rule_id, rule_arguments};
use crate::model::{GenerationSelection, GenerationSelectionSnapshot};
use crate::model_picker::{
    ModelPicker, ModelPickerView, PickerFocus, PickerInput, PickerInputOutcome, PickerOutcome,
};
use crate::rules::{RememberRequest, prepare_from_catalogue};
use ego_core::config::{
    ManagedRuleEffect, Mode, RuleMutation, Trust, UserConfigMutation, UserConfigPublication,
    UserConfigStore,
};
use ego_core::event::domain::CancelCause;
use ego_core::event::id::{ItemId, SessionId};
use ego_core::permit::{CanonPath, PathResolver, PermissionDecision, PermissionRequest, Tool};
use ego_core::question::{
    Elicitor, ElicitorError, QuestionAnswer, QuestionMode, QuestionOption, QuestionOutcome,
    QuestionRequest,
};
use ego_core::state::SessionState;
use ego_core::turn::{
    CancelToken, InteractiveControl, InteractiveIngress, QueueStatus, RegisteredTool,
    interactive_control,
};
use ego_mcp::McpRegistryController;
use ego_tools::fs::OsFs;

const CHANNEL_CAPACITY: usize = 1;
pub(super) const ACTION_COMMAND: &str = "ego:action:";
const ACTION_MENU: &str = "ego_action_menu";
pub(super) const MODEL_PICKER_COMMAND: &str = "ego:model-picker:";
pub(super) const QUESTION_SELECTOR_COMMAND: &str = "ego:question-selector:";
pub(super) const MODAL_ESCAPE_COMMAND: &str = "ego:modal-escape";
const QUEUE_LABEL: &str =
    "Messages to be submitted after next tool call (press esc to interrupt and send immediately)";

#[derive(Clone)]
struct StatusBase {
    session: SessionId,
    mode: Mode,
    cwd: PathBuf,
    trust: Trust,
    git: Option<GitFacts>,
}

#[derive(Clone)]
struct Actions {
    registry: Arc<ActionRegistry>,
    sink: Arc<Mutex<CompositeActionSink>>,
}

#[derive(Debug, Clone, Copy)]
enum SurfaceActionEffect {
    Mode,
    Export,
    Plan,
    Yolo,
    Permissions,
    Compact,
    Fork,
    Sessions,
    Usage,
    New,
    Name,
    Goal,
    Help,
    Clear,
    Statusline,
    Mcp,
    Tools,
    Skills,
    Hooks,
    Doctor,
    Quit,
    Version,
}

#[derive(Clone)]
struct McpSurfaceActions {
    registry: Arc<McpRegistryController>,
    native_tools: Arc<[RegisteredTool]>,
}

struct SurfaceActionSink {
    pending: Arc<Mutex<Option<SurfaceActionEffect>>>,
}

impl ActionSink for SurfaceActionSink {
    fn invoke(&mut self, action: LocalAction) -> LocalActionOutcome {
        let effect = match action.id {
            ActionId::Mode => SurfaceActionEffect::Mode,
            ActionId::Plan => SurfaceActionEffect::Plan,
            ActionId::Yolo => SurfaceActionEffect::Yolo,
            ActionId::Permissions => SurfaceActionEffect::Permissions,
            ActionId::Compact => SurfaceActionEffect::Compact,
            ActionId::Export => SurfaceActionEffect::Export,
            ActionId::Fork => SurfaceActionEffect::Fork,
            ActionId::Sessions => SurfaceActionEffect::Sessions,
            ActionId::Usage => SurfaceActionEffect::Usage,
            ActionId::New => SurfaceActionEffect::New,
            ActionId::Name => SurfaceActionEffect::Name,
            ActionId::Goal => SurfaceActionEffect::Goal,
            ActionId::Help => SurfaceActionEffect::Help,
            ActionId::Clear => SurfaceActionEffect::Clear,
            ActionId::Statusline => SurfaceActionEffect::Statusline,
            ActionId::Mcp => SurfaceActionEffect::Mcp,
            ActionId::Tools => SurfaceActionEffect::Tools,
            ActionId::Skills => SurfaceActionEffect::Skills,
            ActionId::Hooks => SurfaceActionEffect::Hooks,
            ActionId::Doctor => SurfaceActionEffect::Doctor,
            ActionId::Quit => SurfaceActionEffect::Quit,
            ActionId::Version => SurfaceActionEffect::Version,
            _ => {
                return LocalActionOutcome::Refused {
                    reason: "the seat does not own this local action",
                };
            }
        };
        let mut pending = self.pending.lock().expect("surface action effect lock");
        if pending.is_some() {
            return LocalActionOutcome::InFlight;
        }
        *pending = Some(effect);
        LocalActionOutcome::Completed
    }
}

struct ActionCompleter {
    registry: Arc<ActionRegistry>,
}

impl Completer for ActionCompleter {
    fn complete(&mut self, line: &str, position: usize) -> CompletionResult {
        let Some(prefix) = line.get(..position) else {
            return CompletionResult::fresh(Vec::<Suggestion>::new());
        };
        let Some(filter) = prefix.strip_prefix('/') else {
            return CompletionResult::fresh(Vec::<Suggestion>::new());
        };
        if filter.contains(char::is_whitespace) {
            return CompletionResult::fresh(Vec::<Suggestion>::new());
        }
        let mut suggestions = self
            .registry
            .menu(filter)
            .into_iter()
            .map(|registration| Suggestion {
                value: format!("/{}", registration.name()),
                display_override: Some(format!("/{}", registration.name())),
                description: Some(match self.registry.availability(registration.id()) {
                    Availability::Disabled { owner } => {
                        format!("{} — unavailable: {owner}", registration.description_text())
                    }
                    Availability::Busy => {
                        format!("{} — busy", registration.description_text())
                    }
                    Availability::Enabled => registration.description_text().to_string(),
                }),
                span: Span::new(0, position),
                append_whitespace: false,
                ..Suggestion::default()
            })
            .collect::<Vec<_>>();
        suggestions.extend(self.registry.skill_menu(filter).into_iter().map(
            |(name, description)| Suggestion {
                value: format!("/{name}"),
                display_override: Some(format!("/{name}")),
                description: Some(format!("{description} — skill")),
                span: Span::new(0, position),
                append_whitespace: true,
                ..Suggestion::default()
            },
        ));
        CompletionResult::fresh(suggestions)
    }
}

fn rebuild_snapshot(
    status: &mut SurfaceStatus,
    state: &SessionState,
    selection: GenerationSelectionSnapshot,
    modal: bool,
) {
    let Some(base) = status.status_base.clone() else {
        return;
    };
    let seat = SeatFacts::new(
        status.queue,
        status.drafts,
        base.cwd.display().to_string(),
        base.trust,
    )
    .with_modal(modal);
    status.snapshot = Some(SurfaceSnapshot::from_state_with_git(
        base.session,
        state,
        SelectionFacts::new(selection.identity, selection.thinking, base.mode),
        seat,
        base.git,
    ));
}

fn repaint_status(status: &SharedStatus) {
    let (painter, line) = {
        let status = status.lock().expect("surface status lock");
        let painter = Arc::clone(&status.painter);
        let line = status
            .native_status
            .then(|| status.snapshot.as_ref())
            .flatten()
            .map(|snapshot| StatusLine::render(snapshot, &status.configured, status.width));
        (painter, line)
    };
    if let Some(line) = line {
        painter.paint_status(line);
    } else {
        painter.clear_status();
    }
}

struct SurfaceStatus {
    queue: QueueStatus,
    drafts: usize,
    notice: Option<String>,
    origin: InputOrigin,
    native_status: bool,
    width: usize,
    configured: Vec<StatusSegmentName>,
    config_store: Option<Arc<dyn UserConfigStore>>,
    snapshot: Option<SurfaceSnapshot>,
    status_base: Option<StatusBase>,
    painter: Arc<InteractivePainter>,
    actions: Option<Actions>,
    model_picker: Option<Arc<Mutex<ModelPicker>>>,
    model_selection: Option<Arc<GenerationSelection>>,
    question_selector: Option<Arc<Mutex<Selector>>>,
    cursor_selectors: bool,
    mcp_actions: Option<McpSurfaceActions>,
    surface_effect: Arc<Mutex<Option<SurfaceActionEffect>>>,
}

impl Default for SurfaceStatus {
    fn default() -> Self {
        Self {
            queue: QueueStatus::default(),
            drafts: 0,
            notice: None,
            origin: InputOrigin::Typed,
            native_status: false,
            width: 0,
            configured: Vec::new(),
            config_store: None,
            snapshot: None,
            status_base: None,
            painter: Arc::new(InteractivePainter::new()),
            actions: None,
            model_picker: None,
            model_selection: None,
            question_selector: None,
            cursor_selectors: false,
            mcp_actions: None,
            surface_effect: Arc::new(Mutex::new(None)),
        }
    }
}

impl SurfaceStatus {
    fn with_registry(registry: Arc<ActionRegistry>) -> Self {
        Self {
            actions: Some(Actions {
                registry,
                sink: Arc::new(Mutex::new(CompositeActionSink::new())),
            }),
            ..Self::default()
        }
    }
}

type SharedStatus = Arc<Mutex<SurfaceStatus>>;

struct ModalSurface<'a> {
    phase: &'a SurfacePhase,
    status: &'a SharedStatus,
    asked_for_image: &'a AtomicBool,
    ingress: &'a InteractiveIngress,
    drafts: &'a mut DraftStash,
}

struct TerminalOwner {
    phase: SurfacePhase,
    status: SharedStatus,
    asked_for_image: Arc<AtomicBool>,
    ingress: InteractiveIngress,
    cancel: CancelToken,
}

struct ComposerDraft {
    text: String,
    cursor: usize,
    origin: InputOrigin,
}

/// Composer drafts partitioned by the session that owns them.
///
/// Each partition is a stack: activating another session changes only the
/// selected namespace, and returning to a source restores its drafts in LIFO
/// order without copying them into the target.
pub struct DraftStash {
    active: SessionId,
    by_session: HashMap<SessionId, Vec<ComposerDraft>>,
}

impl DraftStash {
    pub fn new(active: SessionId) -> Self {
        Self {
            active,
            by_session: HashMap::new(),
        }
    }

    pub fn active(&self) -> SessionId {
        self.active
    }

    pub fn activate(&mut self, session: SessionId) {
        self.active = session;
    }

    pub fn active_count(&self) -> usize {
        self.count(self.active)
    }

    pub fn count(&self, session: SessionId) -> usize {
        self.by_session.get(&session).map_or(0, Vec::len)
    }

    fn push_active(&mut self, draft: ComposerDraft) {
        self.by_session.entry(self.active).or_default().push(draft);
    }

    fn pop_active(&mut self) -> Option<ComposerDraft> {
        self.by_session.get_mut(&self.active).and_then(Vec::pop)
    }
}

struct SavedComposer {
    text: String,
    cursor: usize,
    origin: InputOrigin,
}

enum GitWork {
    Refresh,
    Shutdown,
}

struct GitWorker {
    sender: Sender<GitWork>,
    owner: std::thread::JoinHandle<()>,
}

/// Complete inputs needed to activate the native status controller.
pub struct StatusSetup {
    pub session: SessionId,
    pub state: SessionState,
    pub selection: GenerationSelectionSnapshot,
    pub mode: Mode,
    pub cwd: PathBuf,
    pub trust: Trust,
    pub segments: Vec<ego_core::config::StatusSegmentName>,
    pub width: usize,
    pub native: bool,
    pub git: Option<Arc<dyn GitFactsSource>>,
}

/// Local and durable input that still exists when the session exits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExitCounts {
    /// Composer buffers explicitly saved in the session-local stash.
    pub drafts: usize,
    /// Durable prompts that have not yet been consumed into conversation.
    pub accepted: usize,
}

/// Whether plain Enter may submit the composer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditPhase {
    Idle = 0,
    Busy = 1,
    Modal = 2,
}

/// Shared phase read directly by the editor thread's key mode.
#[derive(Debug, Clone)]
pub struct SurfacePhase(Arc<AtomicU8>);

impl SurfacePhase {
    pub fn new(phase: EditPhase) -> Self {
        Self(Arc::new(AtomicU8::new(phase as u8)))
    }

    pub fn set(&self, phase: EditPhase) {
        self.0.store(phase as u8, Ordering::Release);
    }

    fn get(&self) -> EditPhase {
        match self.0.load(Ordering::Acquire) {
            0 => EditPhase::Idle,
            2 => EditPhase::Modal,
            _ => EditPhase::Busy,
        }
    }
}

/// The bounded, row-framing producer handed to interactive renderers.
pub struct SurfaceOutput {
    sender: Sender<String>,
    pending: String,
    cancel: CancelToken,
}

impl SurfaceOutput {
    pub fn new(sender: Sender<String>, cancel: CancelToken) -> Self {
        Self {
            sender,
            pending: String::new(),
            cancel,
        }
    }

    pub fn write(&mut self, text: &str) -> Result<(), SendError<String>> {
        self.pending.push_str(text);
        while let Some(end) = self.pending.find('\n').map(|index| index + 1) {
            let row: String = self.pending.drain(..end).collect();
            self.send(row)?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> Result<(), SendError<String>> {
        if !self.pending.is_empty() {
            let row = std::mem::take(&mut self.pending);
            self.send(row)?;
        }

        // `finish` is the barrier before the terminal owner's shutdown. The
        // capacity-one queue makes this empty sentinel wait until Reedline has
        // taken the preceding row. Reedline drains the queue in one loop and
        // an empty string has no visible lines, so the sole owner is committed
        // to painting prior output without introducing another terminal writer.
        self.send(String::new())
    }

    fn send(&self, row: String) -> Result<(), SendError<String>> {
        self.sender.send(row).inspect_err(|_| {
            self.cancel.cancel(CancelCause::Host);
        })
    }
}

enum Control {
    Activate {
        session: SessionId,
    },
    ConfirmCompaction {
        source: SessionId,
        reply: Sender<bool>,
    },
    ConfirmRemember {
        reply: Sender<Confirmation>,
    },
    ActivateSession {
        target: SessionId,
        reply: Sender<bool>,
    },
    Trust {
        workspace: String,
        reply: Sender<Trust>,
    },
    Permission {
        request: PermissionRequest,
        reply: oneshot::Sender<PermissionDecision>,
    },
    Question {
        id: ItemId,
        request: QuestionRequest,
        reply: oneshot::Sender<QuestionOutcome>,
    },
    Shutdown,
}

struct LiveSeat {
    input: Receiver<Line>,
    control: Sender<Control>,
    activated: AtomicBool,
    break_signal: Arc<AtomicBool>,
    phase: SurfacePhase,
    status: SharedStatus,
    git_worker: Mutex<Option<GitWorker>>,
    owner: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl LiveSeat {
    fn wake_owner(&self) {
        if self.activated.load(Ordering::Acquire) {
            self.break_signal.store(true, Ordering::Release);
        }
    }
}

struct SequentialSeat {
    editor: Mutex<Reedline>,
    asked_for_image: Arc<AtomicBool>,
}

enum SeatKind {
    Live(LiveSeat),
    Sequential(Box<SequentialSeat>),
}

/// The human input adapter. A real terminal uses the lifetime owner; piped
/// stdin retains the sequential adapter and its existing byte behavior.
pub struct Seat {
    kind: SeatKind,
    terminal: bool,
    registry: Arc<ActionRegistry>,
    coordinator: Mutex<Option<InteractiveControl>>,
}

impl std::fmt::Debug for Seat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seat")
    }
}

impl Seat {
    /// Build the applicable editor and, for a terminal, its only output port.
    ///
    /// Bracketed paste is not a nicety: without it a pasted stack trace
    /// submits its first line as a whole prompt, which is the failure that
    /// makes pasting one impossible. `CLI.md` §6.1.
    pub fn new(cancel: CancelToken) -> std::io::Result<(Self, Option<SurfaceOutput>)> {
        let (ingress, control) = interactive_control();
        let terminal = std::io::stdin().is_terminal();
        let phase = SurfacePhase::new(if terminal {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        });
        let registry = Arc::new(ActionRegistry::final_surface());
        let status = Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::clone(
            &registry,
        ))));
        let mut keybindings = default_emacs_keybindings();
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu(ACTION_MENU.to_string()),
                ReedlineEvent::MenuNext,
            ]),
        );
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char('/'),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![EditCommand::InsertChar('/')]),
                ReedlineEvent::Menu(ACTION_MENU.to_string()),
            ]),
        );
        let mode = SurfaceEditMode::with_status(
            Emacs::new(keybindings),
            phase.clone(),
            Arc::clone(&status),
        );
        let asked_for_image = mode.asked_for_image();
        let mut editor = Reedline::create()
            .with_edit_mode(Box::new(mode))
            .with_completer(Box::new(ActionCompleter {
                registry: Arc::clone(&registry),
            }))
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(
                ColumnarMenu::default().with_name(ACTION_MENU),
            )))
            .use_bracketed_paste(true);

        if !terminal {
            return Ok((
                Self {
                    kind: SeatKind::Sequential(Box::new(SequentialSeat {
                        editor: Mutex::new(editor),
                        asked_for_image,
                    })),
                    terminal,
                    registry,
                    coordinator: Mutex::new(Some(control)),
                },
                None,
            ));
        }

        let painter = Arc::new(InteractivePainter::live(editor.repaint_signal()));
        {
            let mut status = status.lock().expect("surface status lock");
            status.painter = painter;
            status.cursor_selectors = true;
        }
        let printer = ExternalPrinter::new(CHANNEL_CAPACITY);
        let output = SurfaceOutput::new(printer.sender(), cancel.clone());
        let break_signal = Arc::new(AtomicBool::new(false));
        let (input_sender, input) = bounded(CHANNEL_CAPACITY);
        let (seat_control, control_receiver) = bounded(CHANNEL_CAPACITY);
        let owner_break = Arc::clone(&break_signal);
        let owner_phase = phase.clone();
        let owner_status = Arc::clone(&status);
        let owner = std::thread::Builder::new()
            .name("ego-reedline".to_string())
            .spawn(move || {
                own_terminal(
                    editor
                        .with_external_printer(printer)
                        .with_break_signal(owner_break),
                    input_sender,
                    control_receiver,
                    TerminalOwner {
                        phase: owner_phase,
                        status: owner_status,
                        asked_for_image,
                        ingress,
                        cancel,
                    },
                );
            })?;

        Ok((
            Self {
                kind: SeatKind::Live(LiveSeat {
                    input,
                    control: seat_control,
                    activated: AtomicBool::new(false),
                    break_signal,
                    phase,
                    status,
                    git_worker: Mutex::new(None),
                    owner: Mutex::new(Some(owner)),
                }),
                terminal,
                registry,
                coordinator: Mutex::new(Some(control)),
            },
            Some(output),
        ))
    }

    /// Read one line.
    pub fn read(&self) -> Line {
        match &self.kind {
            SeatKind::Live(live) => {
                let line = live.input.recv().unwrap_or(Line::Ended);
                if matches!(line, Line::Submitted(_)) {
                    live.phase.set(EditPhase::Busy);
                }
                line
            }
            SeatKind::Sequential(seat) => {
                let line = read_with(
                    &seat.editor,
                    &seat.asked_for_image,
                    &Waiting::new(Arc::new(Mutex::new(SurfaceStatus::default()))),
                );
                let Line::Submitted(text) = line else {
                    return line;
                };
                match self
                    .registry
                    .classify(self.registry.submit(text.clone(), InputOrigin::Typed))
                {
                    Dispatch::SessionMetadata(operation) => {
                        Line::HostCommand(HostCommand::SessionMetadata(operation))
                    }
                    Dispatch::Skill(operation) => Line::HostCommand(HostCommand::Skill(operation)),
                    Dispatch::Skills(action) => Line::HostCommand(HostCommand::Skills(action)),
                    Dispatch::Export(selection) => {
                        Line::HostCommand(HostCommand::Export(selection))
                    }
                    Dispatch::Hooks => Line::HostCommand(HostCommand::Hooks),
                    Dispatch::Refused { reason } => Line::Refused(reason),
                    Dispatch::Action(_)
                    | Dispatch::Prompt
                    | Dispatch::Immediate
                    | Dispatch::InteractiveIngress => Line::Submitted(text),
                }
            }
        }
    }

    /// Expose ordinary input only after every interactive authority is wired.
    pub fn activate(&self) {
        if let SeatKind::Live(live) = &self.kind
            && !live.activated.swap(true, Ordering::AcqRel)
        {
            let session = live
                .status
                .lock()
                .expect("surface status lock")
                .status_base
                .as_ref()
                .map(|base| base.session)
                .expect("status is configured before input activation");
            let _ = live.control.send(Control::Activate { session });
        }
    }

    /// Publish the lifecycle reason before waking Reedline out of its read.
    pub fn shutdown(&self) {
        if let SeatKind::Live(live) = &self.kind {
            if let Some(worker) = live.git_worker.lock().expect("Git worker lock").as_ref() {
                let _ = worker.sender.try_send(GitWork::Shutdown);
            }
            let _ = live.control.send(Control::Shutdown);
            live.break_signal.store(true, Ordering::Release);
        }
    }

    /// Join only after every external output producer has been dropped.
    pub fn join(&self) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        if let Some(owner) = live.owner.lock().expect("terminal owner lock").take() {
            let _ = owner.join();
        }
        if let Some(worker) = live.git_worker.lock().expect("Git worker lock").take() {
            drop(worker.sender);
            let _ = worker.owner.join();
        }
    }

    /// Snapshot the exact local-draft and durable-input remainder.
    pub fn exit_counts(&self) -> ExitCounts {
        let SeatKind::Live(live) = &self.kind else {
            return ExitCounts::default();
        };
        let status = live.status.lock().expect("surface status lock");
        ExitCounts {
            drafts: status.drafts,
            accepted: status.queue.pending_steers + status.queue.queued,
        }
    }

    /// Install one complete local-action topology around the shared picker and
    /// live MCP registry authority.
    pub fn install_model_picker(
        &self,
        picker: Arc<Mutex<ModelPicker>>,
        selection: Arc<GenerationSelection>,
        config_store: Arc<dyn UserConfigStore>,
        mcp_registry: Arc<McpRegistryController>,
        native_tools: Vec<RegisteredTool>,
    ) -> Result<(), CompositeActionSinkError> {
        let SeatKind::Live(live) = &self.kind else {
            return Ok(());
        };
        let mut sink = CompositeActionSink::new();
        sink.register(
            ActionId::Model,
            Box::new(SharedActionSink::new(Arc::clone(&picker))),
        )?;
        sink.register(
            ActionId::Thinking,
            Box::new(SharedActionSink::new(Arc::clone(&picker))),
        )?;
        let surface_effect = Arc::new(Mutex::new(None));
        let surface_sink = Arc::new(Mutex::new(SurfaceActionSink {
            pending: Arc::clone(&surface_effect),
        }));
        for action in [
            ActionId::Mode,
            ActionId::Plan,
            ActionId::Yolo,
            ActionId::Permissions,
            ActionId::Compact,
            ActionId::Export,
            ActionId::Fork,
            ActionId::Sessions,
            ActionId::Usage,
            ActionId::New,
            ActionId::Name,
            ActionId::Goal,
            ActionId::Help,
            ActionId::Clear,
            ActionId::Statusline,
            ActionId::Mcp,
            ActionId::Tools,
            ActionId::Skills,
            ActionId::Hooks,
            ActionId::Doctor,
            ActionId::Quit,
            ActionId::Version,
        ] {
            sink.register(
                action,
                Box::new(SharedActionSink::new(Arc::clone(&surface_sink))),
            )?;
        }
        sink.validate_for(&self.registry)?;
        let actions = live
            .status
            .lock()
            .expect("surface status lock")
            .actions
            .clone()
            .expect("the terminal seat owns its action registry");
        {
            let mut status = live.status.lock().expect("surface status lock");
            status.model_picker = Some(picker);
            status.model_selection = Some(selection);
            status.config_store = Some(config_store);
            status.mcp_actions = Some(McpSurfaceActions {
                registry: mcp_registry,
                native_tools: native_tools.into(),
            });
            status.surface_effect = surface_effect;
        }
        *actions.sink.lock().expect("local action sink lock") = sink;
        Ok(())
    }

    /// Share the exact registry instance used by completion and dispatch.
    pub fn action_registry(&self) -> Arc<ActionRegistry> {
        Arc::clone(&self.registry)
    }

    /// Publish the coordinator-owned compaction guard to every action view.
    pub fn set_compaction_availability(&self, availability: Availability) {
        self.registry
            .set_availability(ActionId::Compact, availability);
        if let SeatKind::Live(live) = &self.kind {
            repaint_status(&live.status);
        }
    }

    /// Enable policy controls after their application owner and catalogue exist.
    pub(crate) fn enable_policy_controls(&self) {
        for action in [ActionId::Mode, ActionId::Plan, ActionId::Yolo] {
            self.registry
                .set_availability(action, Availability::Enabled);
        }
        if let SeatKind::Live(live) = &self.kind {
            if live
                .status
                .lock()
                .expect("surface status lock")
                .mcp_actions
                .is_some()
            {
                self.registry
                    .set_availability(ActionId::Permissions, Availability::Enabled);
            }
            repaint_status(&live.status);
        }
    }

    /// Replace only the active generation mode used by the next status rebuild.
    pub(crate) fn set_status_mode(&self, mode: Mode) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        if let Some(base) = live
            .status
            .lock()
            .expect("surface status lock")
            .status_base
            .as_mut()
        {
            base.mode = mode;
        }
    }

    /// Publish one local controller outcome without creating a prompt.
    pub(crate) fn set_notice(&self, notice: String) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        live.status.lock().expect("surface status lock").notice = Some(notice);
        repaint_status(&live.status);
    }

    /// Activate native status after session, selection and config are known.
    pub fn configure_status(&self, setup: StatusSetup) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        if let Some(worker) = live.git_worker.lock().expect("Git worker lock").take() {
            let _ = worker.sender.try_send(GitWork::Shutdown);
            drop(worker.sender);
            let _ = worker.owner.join();
        }
        let StatusSetup {
            session,
            state,
            selection,
            mode,
            cwd,
            trust,
            segments,
            width,
            native,
            git,
        } = setup;
        {
            let mut status = live.status.lock().expect("surface status lock");
            status.native_status = native && !segments.is_empty();
            status.width = width;
            status.configured = segments.into_iter().map(StatusSegmentName::from).collect();
            status.status_base = Some(StatusBase {
                session,
                mode,
                cwd,
                trust,
                git: None,
            });
            rebuild_snapshot(
                &mut status,
                &state,
                selection,
                live.phase.get() == EditPhase::Modal,
            );
        }
        repaint_status(&live.status);

        let git_enabled = {
            let status = live.status.lock().expect("surface status lock");
            status.native_status && status.configured.contains(&StatusSegmentName::Git)
        };
        if !git_enabled || git.is_none() {
            return;
        }
        let source = git.expect("checked above");
        let (sender, receiver) = bounded(CHANNEL_CAPACITY);
        let status = Arc::clone(&live.status);
        let owner = std::thread::Builder::new()
            .name("ego-status-git".to_string())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    match work {
                        GitWork::Shutdown => break,
                        GitWork::Refresh => {
                            let result = source.collect();
                            {
                                let mut status = status.lock().expect("surface status lock");
                                match result {
                                    Ok(git) => {
                                        if let Some(base) = status.status_base.as_mut() {
                                            base.git = Some(git.clone());
                                        }
                                        if let Some(snapshot) = status.snapshot.as_mut() {
                                            snapshot.set_git(git);
                                        }
                                    }
                                    Err(_) => {
                                        if let Some(base) = status.status_base.as_mut() {
                                            base.git = None;
                                        }
                                        if let Some(snapshot) = status.snapshot.as_mut() {
                                            snapshot.set_git_failed();
                                        }
                                    }
                                }
                            }
                            repaint_status(&status);
                        }
                    }
                }
            });
        match owner {
            Ok(owner) => {
                let worker = GitWorker { sender, owner };
                let _ = worker.sender.try_send(GitWork::Refresh);
                *live.git_worker.lock().expect("Git worker lock") = Some(worker);
            }
            Err(error) => {
                let mut status = live.status.lock().expect("surface status lock");
                status.notice = Some(format!("Git status worker could not start: {error}"));
                if let Some(snapshot) = status.snapshot.as_mut() {
                    snapshot.set_git_failed();
                }
                drop(status);
                repaint_status(&live.status);
            }
        }
    }

    /// Replace state/selection facts and request one coalesced Git refresh.
    pub fn refresh_status(&self, state: &SessionState, selection: GenerationSelectionSnapshot) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        {
            let mut status = live.status.lock().expect("surface status lock");
            rebuild_snapshot(
                &mut status,
                state,
                selection,
                live.phase.get() == EditPhase::Modal,
            );
        }
        repaint_status(&live.status);
        if let Some(worker) = live.git_worker.lock().expect("Git worker lock").as_ref() {
            let _ = worker.sender.try_send(GitWork::Refresh);
        }
    }

    /// Ask one exact durable question through the terminal-owner rendezvous.
    pub fn question(&self, id: ItemId, request: QuestionRequest) -> QuestionOutcome {
        futures::executor::block_on(self.elicit(id, request))
            .unwrap_or(QuestionOutcome::Unavailable)
    }

    pub(crate) fn confirm_remember(&self) -> Confirmation {
        self.request(
            |reply| Control::ConfirmRemember { reply },
            Confirmation::Declined,
        )
    }

    fn request<R>(&self, control: impl FnOnce(Sender<R>) -> Control, fallback: R) -> R {
        let SeatKind::Live(live) = &self.kind else {
            return fallback;
        };
        let (reply, answer) = bounded(0);
        if live.control.send(control(reply)).is_err() {
            return fallback;
        }
        live.wake_owner();
        answer.recv().unwrap_or(fallback)
    }
}

fn read_with(editor: &Mutex<Reedline>, asked_for_image: &AtomicBool, prompt: &dyn Prompt) -> Line {
    let signal = editor.lock().expect("line editor lock").read_line(prompt);
    let asked_for_image = asked_for_image.swap(false, Ordering::Relaxed);
    answer(asked_for_image, signal)
}

impl crate::run::Reads for Seat {
    fn read(&self) -> Line {
        Seat::read(self)
    }
}

impl crate::run::InteractiveSeat for Seat {
    fn has_human(&self) -> bool {
        self.terminal
    }

    fn busy(&self) {
        self.registry
            .set_availability(ActionId::Compact, Availability::Busy);
        if let SeatKind::Live(live) = &self.kind {
            live.phase.set(EditPhase::Busy);
        }
    }

    fn idle(&self) {
        if let SeatKind::Live(live) = &self.kind {
            live.phase.set(EditPhase::Idle);
        }
    }

    fn take_control(&self) -> InteractiveControl {
        self.coordinator
            .lock()
            .expect("interactive coordinator lock")
            .take()
            .expect("the session takes its interactive coordinator once")
    }

    fn refresh_queue(&self, queue: QueueStatus) {
        if let SeatKind::Live(live) = &self.kind {
            let mut status = live.status.lock().expect("surface status lock");
            status.queue = queue;
            if let Some(snapshot) = status.snapshot.as_mut() {
                snapshot.set_queue(queue);
            }
            drop(status);
            repaint_status(&live.status);
        }
    }

    fn set_compaction_availability(&self, availability: Availability) {
        Seat::set_compaction_availability(self, availability);
    }

    fn confirm_compaction(&self, source: SessionId) -> bool {
        self.request(|reply| Control::ConfirmCompaction { source, reply }, false)
    }

    fn activate_session(&self, target: SessionId) -> bool {
        self.request(|reply| Control::ActivateSession { target, reply }, false)
    }

    fn trust(&self, workspace: &Path) -> Trust {
        // Piped stdin may itself be the first prompt. Consuming it as a trust
        // answer would turn user content into authority. With no terminal
        // there is no human seat, so the only truthful answer is untrusted.
        if !self.terminal {
            return Trust::Untrusted;
        }
        self.request(
            |reply| Control::Trust {
                workspace: workspace.display().to_string(),
                reply,
            },
            Trust::Untrusted,
        )
    }

    fn permission(&self, _request: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }

    fn permission_future<'a>(
        &'a self,
        request: &'a PermissionRequest,
    ) -> BoxFuture<'a, PermissionDecision> {
        let SeatKind::Live(live) = &self.kind else {
            return Box::pin(futures::future::ready(PermissionDecision::Deny));
        };
        let (reply, answer) = oneshot::channel();
        if live
            .control
            .send(Control::Permission {
                request: request.clone(),
                reply,
            })
            .is_err()
        {
            return Box::pin(futures::future::ready(PermissionDecision::Deny));
        }
        live.wake_owner();
        Box::pin(async move { answer.await.unwrap_or(PermissionDecision::Deny) })
    }
}

impl Elicitor for Seat {
    fn elicit<'a>(
        &'a self,
        question: ItemId,
        request: QuestionRequest,
    ) -> BoxFuture<'a, Result<QuestionOutcome, ElicitorError>> {
        let SeatKind::Live(live) = &self.kind else {
            return Box::pin(futures::future::ready(Ok(QuestionOutcome::Unavailable)));
        };
        let (reply, answer) = oneshot::channel();
        if live
            .control
            .send(Control::Question {
                id: question,
                request,
                reply,
            })
            .is_err()
        {
            return Box::pin(futures::future::ready(Err(ElicitorError::new(
                "terminal owner is unavailable",
            ))));
        }
        live.wake_owner();
        Box::pin(async move {
            answer
                .await
                .map_err(|_| ElicitorError::new("terminal owner dropped the question"))
        })
    }
}

fn own_terminal(
    mut editor: Reedline,
    input: Sender<Line>,
    control: Receiver<Control>,
    owner: TerminalOwner,
) {
    let TerminalOwner {
        phase,
        status,
        asked_for_image,
        ingress,
        cancel,
    } = owner;
    let mut pending_control = None;
    let mut active = false;
    let mut drafts = DraftStash::new(SessionId::new());
    loop {
        if matches!(pending_control.as_ref(), Some(Control::Activate { .. })) {
            let Some(Control::Activate { session }) = pending_control.take() else {
                unreachable!("the pending activation was just matched");
            };
            drafts.activate(session);
            active = true;
            continue;
        }
        if !active && pending_control.is_none() {
            match control.recv() {
                Ok(Control::Activate { session }) => {
                    drafts.activate(session);
                    active = true;
                    continue;
                }
                Ok(Control::Shutdown) | Err(_) => break,
                Ok(request) => pending_control = Some(request),
            }
        }
        if let Some(request) = pending_control.take() {
            let mut surface = ModalSurface {
                phase: &phase,
                status: &status,
                asked_for_image: &asked_for_image,
                ingress: &ingress,
                drafts: &mut drafts,
            };
            if !answer_control(
                &mut editor,
                &phase,
                &mut surface,
                &control,
                &mut pending_control,
                request,
            ) {
                break;
            }
            continue;
        }

        let (line, failed) = surface_line(
            &mut editor,
            &Waiting::new(Arc::clone(&status)),
            &phase,
            &status,
            &asked_for_image,
            &ingress,
            &mut drafts,
        );
        if failed {
            cancel.cancel(CancelCause::Host);
        }
        match line {
            Line::ExternalBreak(_) => match control.recv() {
                Ok(Control::Shutdown) | Err(_) => break,
                Ok(Control::Activate { session }) => {
                    drafts.activate(session);
                    continue;
                }
                Ok(request) => {
                    let mut surface = ModalSurface {
                        phase: &phase,
                        status: &status,
                        asked_for_image: &asked_for_image,
                        ingress: &ingress,
                        drafts: &mut drafts,
                    };
                    if !answer_control(
                        &mut editor,
                        &phase,
                        &mut surface,
                        &control,
                        &mut pending_control,
                        request,
                    ) {
                        break;
                    }
                }
            },
            Line::HostCommand(HostCommand::Interrupt) => continue,
            Line::Submitted(_)
            | Line::Blank
            | Line::Interrupted
            | Line::Ended
            | Line::Refused(_)
            | Line::HostCommand(_) => {
                if matches!(line, Line::Submitted(_) | Line::Ended) {
                    phase.set(EditPhase::Busy);
                }
                let ended = matches!(line, Line::Ended);
                if input.send(line).is_err() || (ended && failed) {
                    break;
                }
            }
        }
    }
}

fn surface_line(
    editor: &mut Reedline,
    prompt: &dyn Prompt,
    phase: &SurfacePhase,
    status: &SharedStatus,
    asked_for_image: &AtomicBool,
    ingress: &InteractiveIngress,
    drafts: &mut DraftStash,
) -> (Line, bool) {
    loop {
        let signal = editor.read_line(prompt);
        let failed = signal.is_err();
        let line = answer(asked_for_image.swap(false, Ordering::Relaxed), signal);
        if let Line::Submitted(text) = &line
            && phase.get() != EditPhase::Modal
        {
            let dispatch = {
                let mut current = status.lock().expect("surface status lock");
                let dispatch = current
                    .actions
                    .as_ref()
                    .map_or(Dispatch::Prompt, |actions| {
                        let submission = actions.registry.submit(text.clone(), current.origin);
                        actions.registry.classify(submission)
                    });
                current.origin = InputOrigin::Typed;
                dispatch
            };
            match dispatch {
                Dispatch::Action(action) => {
                    let (handled, interrupted) = execute_action_and_modal(
                        editor,
                        phase,
                        status,
                        ingress,
                        action,
                        phase.get() == EditPhase::Busy,
                    );
                    if let Some(interrupted) = interrupted {
                        return (interrupted, false);
                    }
                    if !handled {
                        return (line, failed);
                    }
                    continue;
                }
                Dispatch::SessionMetadata(operation) => {
                    if let Some(line) =
                        route_session_metadata(status, operation, phase.get() == EditPhase::Busy)
                    {
                        return (line, failed);
                    }
                    continue;
                }
                Dispatch::Skill(operation) => {
                    if phase.get() == EditPhase::Busy {
                        status.lock().expect("surface status lock").notice =
                            Some("a skill cannot be invoked while a turn is running".to_owned());
                        repaint_status(status);
                        continue;
                    }
                    return (Line::HostCommand(HostCommand::Skill(operation)), failed);
                }
                Dispatch::Skills(action) => {
                    if phase.get() == EditPhase::Busy {
                        status.lock().expect("surface status lock").notice = Some(
                            "skill actions are unavailable while a turn is running".to_owned(),
                        );
                        repaint_status(status);
                        continue;
                    }
                    return (Line::HostCommand(HostCommand::Skills(action)), failed);
                }
                Dispatch::Export(selection) => {
                    if phase.get() == EditPhase::Busy {
                        status.lock().expect("surface status lock").notice =
                            Some("export is unavailable while a turn is running".to_owned());
                        repaint_status(status);
                        continue;
                    }
                    return (Line::HostCommand(HostCommand::Export(selection)), failed);
                }
                Dispatch::Hooks => {
                    if phase.get() == EditPhase::Busy {
                        status.lock().expect("surface status lock").notice = Some(
                            "hook inspection is unavailable while a turn is running".to_owned(),
                        );
                        repaint_status(status);
                        continue;
                    }
                    return (Line::HostCommand(HostCommand::Hooks), failed);
                }
                Dispatch::Refused { reason } => {
                    status.lock().expect("surface status lock").notice = Some(reason.to_owned());
                    repaint_status(status);
                    continue;
                }
                Dispatch::Prompt | Dispatch::Immediate | Dispatch::InteractiveIngress => {
                    return (line, failed);
                }
            }
        }
        let Line::HostCommand(command) = line else {
            if matches!(line, Line::Blank | Line::Interrupted) {
                status.lock().expect("surface status lock").origin = InputOrigin::Typed;
            }
            return (line, failed);
        };

        if let HostCommand::Action(action) = &command {
            let (_, interrupted) = execute_action_and_modal(
                editor,
                phase,
                status,
                ingress,
                *action,
                phase.get() == EditPhase::Busy,
            );
            if let Some(interrupted) = interrupted {
                return (interrupted, false);
            }
            continue;
        }

        match command {
            HostCommand::Draft => {
                let buffer = editor.current_buffer_contents().to_string();
                if buffer.is_empty() {
                    if let Some(saved) = drafts.pop_active() {
                        editor.run_edit_commands(&[
                            EditCommand::InsertString(saved.text),
                            EditCommand::MoveToPosition {
                                position: saved.cursor,
                                select: false,
                            },
                        ]);
                        status.lock().expect("surface status lock").origin = saved.origin;
                    }
                } else {
                    let origin = status.lock().expect("surface status lock").origin;
                    drafts.push_active(ComposerDraft {
                        text: buffer,
                        cursor: editor.current_insertion_point(),
                        origin,
                    });
                    editor.run_edit_commands(&[EditCommand::Clear]);
                    status.lock().expect("surface status lock").origin = InputOrigin::Typed;
                }
                let mut current = status.lock().expect("surface status lock");
                current.drafts = drafts.active_count();
                let draft_count = current.drafts;
                if let Some(snapshot) = current.snapshot.as_mut() {
                    snapshot.set_drafts(draft_count);
                }
                current.notice = None;
                drop(current);
                repaint_status(status);
            }
            HostCommand::BusySubmit => {
                let buffer = editor.current_buffer_contents().to_string();
                if buffer.trim().is_empty() {
                    continue;
                }
                let dispatch = {
                    let status = status.lock().expect("surface status lock");
                    status.actions.as_ref().map_or(Dispatch::Prompt, |actions| {
                        actions
                            .registry
                            .classify(actions.registry.submit(buffer.clone(), status.origin))
                    })
                };
                match dispatch {
                    Dispatch::Action(action) => {
                        let (handled, interrupted) =
                            execute_action_and_modal(editor, phase, status, ingress, action, true);
                        if let Some(interrupted) = interrupted {
                            return (interrupted, false);
                        }
                        if handled {
                            editor.run_edit_commands(&[EditCommand::Clear]);
                            status.lock().expect("surface status lock").origin = InputOrigin::Typed;
                        }
                        continue;
                    }
                    Dispatch::SessionMetadata(operation) => {
                        route_session_metadata(status, operation, true);
                        continue;
                    }
                    Dispatch::Skill(_) => {
                        status.lock().expect("surface status lock").notice =
                            Some("a skill cannot be invoked while a turn is running".to_owned());
                        repaint_status(status);
                        continue;
                    }
                    Dispatch::Skills(_) => {
                        status.lock().expect("surface status lock").notice = Some(
                            "skill actions are unavailable while a turn is running".to_owned(),
                        );
                        repaint_status(status);
                        continue;
                    }
                    Dispatch::Export(_) => {
                        // The same refusal the shortcut gives while busy. A
                        // transcript exported mid-turn would be a snapshot of a
                        // conversation that is still moving.
                        status.lock().expect("surface status lock").notice =
                            Some("export is unavailable while a turn is running".to_owned());
                        repaint_status(status);
                        continue;
                    }
                    Dispatch::Hooks => {
                        status.lock().expect("surface status lock").notice = Some(
                            "hook inspection is unavailable while a turn is running".to_owned(),
                        );
                        repaint_status(status);
                        continue;
                    }
                    Dispatch::Refused { reason } => {
                        status.lock().expect("surface status lock").notice =
                            Some(reason.to_owned());
                        repaint_status(status);
                        continue;
                    }
                    Dispatch::Prompt | Dispatch::Immediate | Dispatch::InteractiveIngress => {}
                }
                match futures::executor::block_on(ingress.submit(buffer)) {
                    Ok(queue) => {
                        editor.run_edit_commands(&[EditCommand::Clear]);
                        let mut current = status.lock().expect("surface status lock");
                        current.queue = queue;
                        current.origin = InputOrigin::Typed;
                        if let Some(snapshot) = current.snapshot.as_mut() {
                            snapshot.set_queue(queue);
                        }
                        current.notice = None;
                        drop(current);
                        repaint_status(status);
                    }
                    Err(error) => {
                        status.lock().expect("surface status lock").notice =
                            Some(error.to_string());
                    }
                }
            }
            HostCommand::Interrupt => match futures::executor::block_on(ingress.interrupt()) {
                Ok(queue) => {
                    let mut current = status.lock().expect("surface status lock");
                    current.queue = queue;
                    if let Some(snapshot) = current.snapshot.as_mut() {
                        snapshot.set_queue(queue);
                    }
                    current.notice = None;
                    drop(current);
                    repaint_status(status);
                    return (Line::HostCommand(HostCommand::Interrupt), false);
                }
                Err(error) => {
                    status.lock().expect("surface status lock").notice = Some(error.to_string());
                }
            },
            command => return (Line::HostCommand(command), failed),
        }

        // A suspended read retains the exact buffer and cursor. Starting it
        // again repaints the state changed by the local action above.
    }
}

fn execute_action(
    status: &SharedStatus,
    ingress: &InteractiveIngress,
    action: ActionId,
    busy: bool,
) -> (bool, bool) {
    let Some(actions) = status.lock().expect("surface status lock").actions.clone() else {
        return (false, false);
    };
    let displayed = actions.registry.snapshot();
    let Some(selection) = actions.registry.select(action, &displayed) else {
        return (false, false);
    };
    let current = actions.registry.snapshot();
    let dispatch = {
        let mut sink = actions.sink.lock().expect("local action sink lock");
        actions
            .registry
            .dispatch_to(selection, &current, busy, &mut *sink)
    };
    let (handled, completed, notice) = match dispatch {
        ActionDispatch::Local(LocalActionOutcome::Completed) => (true, true, None),
        ActionDispatch::Local(LocalActionOutcome::InFlight) => {
            (true, false, Some("action is already in flight".to_string()))
        }
        ActionDispatch::Local(LocalActionOutcome::Refused { reason })
        | ActionDispatch::Refused { reason } => (false, false, Some(reason.to_string())),
        ActionDispatch::Prompt(payload) => {
            let canonical = format!("/{}", actions.registry.help(payload.id).name());
            match futures::executor::block_on(ingress.submit(canonical)) {
                Ok(queue) => {
                    let mut current = status.lock().expect("surface status lock");
                    current.queue = queue;
                    if let Some(snapshot) = current.snapshot.as_mut() {
                        snapshot.set_queue(queue);
                    }
                    (true, true, None)
                }
                Err(error) => (false, false, Some(error.to_string())),
            }
        }
    };
    status.lock().expect("surface status lock").notice = notice;
    repaint_status(status);
    (handled, completed)
}

fn execute_action_and_modal(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    ingress: &InteractiveIngress,
    action: ActionId,
    busy: bool,
) -> (bool, Option<Line>) {
    let (handled, completed) = execute_action(status, ingress, action, busy);
    if completed {
        let effect = {
            let effects = status
                .lock()
                .expect("surface status lock")
                .surface_effect
                .clone();
            effects.lock().expect("surface action effect lock").take()
        };
        if let Some(line) =
            effect.and_then(|effect| apply_surface_effect(editor, phase, status, effect, busy))
        {
            return (true, Some(line));
        }
    }
    if handled && !completed {
        return (true, None);
    }
    if !matches!(action, ActionId::Model | ActionId::Thinking) {
        return (handled, None);
    }
    let picker = {
        let status = status.lock().expect("surface status lock");
        status.model_picker.clone()
    };
    let Some(picker) = picker else {
        return (handled, None);
    };
    if !handled {
        let diagnostic = picker
            .lock()
            .expect("model picker lock")
            .diagnostic()
            .map(ToString::to_string);
        if let Some(diagnostic) = diagnostic {
            status.lock().expect("surface status lock").notice = Some(diagnostic);
            repaint_status(status);
        }
        return (false, None);
    }
    if !picker.lock().expect("model picker lock").is_open() {
        return (handled, None);
    }
    (
        handled,
        run_model_picker(editor, phase, status, picker, busy),
    )
}

fn apply_surface_effect(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    effect: SurfaceActionEffect,
    busy: bool,
) -> Option<Line> {
    match effect {
        SurfaceActionEffect::Mode => {
            return choose_policy_mode(editor, phase, status, busy);
        }
        SurfaceActionEffect::Plan => {
            let mut controller = PolicyControlController::new();
            return controller
                .select_mode(Mode::Plan)
                .map(|request| Line::HostCommand(HostCommand::Policy(request)));
        }
        SurfaceActionEffect::Yolo => {
            let mut controller = PolicyControlController::new();
            controller.begin_yolo();
            return confirm_yolo(editor, phase, status, busy, &mut controller);
        }
        SurfaceActionEffect::Permissions => {
            return choose_permission_action(editor, phase, status, busy);
        }
        SurfaceActionEffect::Compact => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Compact)));
        }
        SurfaceActionEffect::Fork => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Fork)));
        }
        SurfaceActionEffect::Sessions => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Sessions)));
        }
        SurfaceActionEffect::Usage => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Usage)));
        }
        SurfaceActionEffect::New => {
            return Some(Line::HostCommand(HostCommand::Action(ActionId::New)));
        }
        SurfaceActionEffect::Export => {
            if busy {
                status.lock().expect("surface status lock").notice =
                    Some("export is unavailable while a turn is running".to_owned());
            } else {
                // The shortcut and the menu carry no words, so they select the
                // default the slash grammar documents: sanitized markdown.
                return Some(Line::HostCommand(HostCommand::Export(
                    crate::repl::actions::ExportSelection::default(),
                )));
            }
        }
        SurfaceActionEffect::Name => {
            return route_session_metadata(
                status,
                crate::cli::SessionMetadataOperation::Name(crate::cli::NameOperation::Show),
                busy,
            );
        }
        SurfaceActionEffect::Goal => {
            return route_session_metadata(
                status,
                crate::cli::SessionMetadataOperation::Goal(crate::cli::GoalOperation::Show),
                busy,
            );
        }
        SurfaceActionEffect::Help => {
            let help = {
                let current = status.lock().expect("surface status lock");
                current.actions.as_ref().map(|actions| {
                    actions
                        .registry
                        .iter()
                        .map(|action| {
                            let shortcut = action
                                .shortcuts()
                                .first()
                                .map(|shortcut| format!(" [Meta-{shortcut}]"))
                                .unwrap_or_default();
                            match actions.registry.availability(action.id()) {
                                Availability::Disabled { owner } => {
                                    format!(
                                        "/{}{shortcut} — {} [unavailable: {owner}]",
                                        action.name(),
                                        action.description_text()
                                    )
                                }
                                Availability::Busy => format!(
                                    "/{}{shortcut} — {} [busy]",
                                    action.name(),
                                    action.description_text()
                                ),
                                Availability::Enabled => format!(
                                    "/{}{shortcut} — {}",
                                    action.name(),
                                    action.description_text()
                                ),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            };
            status.lock().expect("surface status lock").notice =
                help.map(|help| format!("actions: {help}"));
        }
        SurfaceActionEffect::Clear => {
            let buffer = editor.current_buffer_contents().to_string();
            if !buffer.trim().is_empty() && buffer.trim() != "/clear" {
                match confirm_surface_action(
                    editor,
                    phase,
                    status,
                    "Clear the unsent composer draft? [y/N] ",
                    busy,
                ) {
                    Confirmation::Confirmed => {}
                    Confirmation::Declined => {
                        status.lock().expect("surface status lock").notice =
                            Some("composer clear cancelled".to_string());
                        repaint_status(status);
                        return None;
                    }
                    Confirmation::Propagate(line) => return Some(line),
                }
            }
            editor.run_edit_commands(&[EditCommand::Clear]);
            status.lock().expect("surface status lock").notice = None;
        }
        SurfaceActionEffect::Statusline => {
            let segments = match choose_statusline_preset(editor, phase, status, busy) {
                StatuslineChoice::Selected(segments) => segments,
                StatuslineChoice::Cancelled => return None,
                StatuslineChoice::Propagate(line) => return Some(line),
            };
            configure_statusline(status, segments);
            return None;
        }
        SurfaceActionEffect::Mcp => {
            let actions = status
                .lock()
                .expect("surface status lock")
                .mcp_actions
                .clone()
                .expect("the enabled MCP action has an installed handler");
            let registry_status = actions.registry.status();
            let notice = if registry_status.servers().is_empty() {
                "mcp: no admitted servers".to_string()
            } else {
                format!("mcp: {:?}", registry_status.servers())
            };
            status.lock().expect("surface status lock").notice = Some(notice);
        }
        SurfaceActionEffect::Tools => {
            let actions = status
                .lock()
                .expect("surface status lock")
                .mcp_actions
                .clone()
                .expect("the enabled tools action has an installed handler");
            let notice = match actions.registry.capture() {
                Ok(captured) => {
                    let native = actions
                        .native_tools
                        .iter()
                        .map(|tool| format!("native/{}", tool.descriptor().name));
                    let foreign = captured.tools().iter().map(|tool| {
                        format!("mcp:{}/{}", captured.generation(), tool.descriptor().name)
                    });
                    let catalogue = native.chain(foreign).collect::<Vec<_>>();
                    if catalogue.is_empty() {
                        "tools: none".to_string()
                    } else {
                        format!("tools: {}", catalogue.join(" "))
                    }
                }
                Err(_) => "tools: the admitted tool catalogue is unavailable".to_string(),
            };
            status.lock().expect("surface status lock").notice = Some(notice);
        }
        SurfaceActionEffect::Skills => {
            if busy {
                status.lock().expect("surface status lock").notice =
                    Some("skill actions are unavailable while a turn is running".to_owned());
            } else {
                return Some(Line::HostCommand(HostCommand::Skills(
                    SkillSurfaceAction::List,
                )));
            }
        }
        SurfaceActionEffect::Hooks => {
            if busy {
                status.lock().expect("surface status lock").notice =
                    Some("hook inspection is unavailable while a turn is running".to_owned());
            } else {
                return Some(Line::HostCommand(HostCommand::Hooks));
            }
        }
        SurfaceActionEffect::Doctor => {
            // The whole snapshot, not the MCP corner of one. `/doctor` and
            // `ego doctor` render the same value now, so an operator cannot be
            // told two different things about the same installation depending
            // on which surface they happened to ask from.
            return Some(Line::HostCommand(HostCommand::Action(ActionId::Doctor)));
        }
        SurfaceActionEffect::Quit => {
            let (drafts, queue) = {
                let current = status.lock().expect("surface status lock");
                (
                    current.drafts,
                    current.queue.pending_steers + current.queue.queued,
                )
            };
            let composer = editor.current_buffer_contents().trim();
            let composer_pending = !composer.is_empty() && composer != "/quit";
            if composer_pending || drafts > 0 || queue > 0 {
                let prompt = format!(
                    "Exit with {} composer draft, {drafts} saved drafts, and {queue} accepted prompts pending? [y/N] ",
                    usize::from(composer_pending)
                );
                match confirm_surface_action(editor, phase, status, &prompt, busy) {
                    Confirmation::Confirmed => {}
                    Confirmation::Declined => {
                        status.lock().expect("surface status lock").notice =
                            Some("quit cancelled; all unsent input was retained".to_string());
                        repaint_status(status);
                        return None;
                    }
                    Confirmation::Propagate(line) => return Some(line),
                }
            }
            return Some(Line::Ended);
        }
        SurfaceActionEffect::Version => {
            status.lock().expect("surface status lock").notice = Some(crate::build_info::summary());
        }
    }
    repaint_status(status);
    None
}

fn route_session_metadata(
    status: &SharedStatus,
    operation: crate::cli::SessionMetadataOperation,
    busy: bool,
) -> Option<Line> {
    if busy {
        status.lock().expect("surface status lock").notice =
            Some("session metadata is unavailable while a turn is running".to_owned());
        repaint_status(status);
        None
    } else {
        Some(Line::HostCommand(HostCommand::SessionMetadata(operation)))
    }
}

enum PolicyModeChoice {
    Selected(Mode),
    Cancelled,
    Propagate(Line),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionMenuId {
    List,
    Allow,
    Deny,
    Revoke,
    Remember,
    Explain,
}

impl PermissionMenuId {
    fn as_str(&self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Revoke => "revoke",
            Self::Remember => "remember",
            Self::Explain => "explain",
        }
    }
}

fn permission_menu() -> [PermissionMenuId; 6] {
    [
        PermissionMenuId::List,
        PermissionMenuId::Allow,
        PermissionMenuId::Deny,
        PermissionMenuId::Revoke,
        PermissionMenuId::Remember,
        PermissionMenuId::Explain,
    ]
}

enum PermissionMenuChoice {
    Selected(PermissionMenuId),
    Cancelled,
    Propagate(Line),
}

enum PermissionField {
    Value(String),
    Cancelled,
    Propagate(Line),
}

fn choose_permission_action(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
) -> Option<Line> {
    let choices = permission_menu();
    let request = QuestionRequest::new(
        "Choose permission operation",
        QuestionMode::Single,
        choices
            .iter()
            .map(|choice| QuestionOption::new(choice.as_str(), choice.as_str()))
            .collect(),
        false,
    );
    let selector = Arc::new(Mutex::new(Selector::with_options(
        SelectorKind::Single,
        request
            .options
            .iter()
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone())),
    )));
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(status);

    let choice = loop {
        let prompt = Question::new(
            question_selector_prompt(&request, &selector),
            Arc::clone(status),
        );
        match answer(false, editor.read_line(&prompt)) {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = question_selector_key(&command) else {
                    break PermissionMenuChoice::Cancelled;
                };
                let mut selector = selector.lock().expect("permission selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break PermissionMenuChoice::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    repaint_status(status);
                    continue;
                }
                let Some(SelectorValue::Single(id)) = selector.result() else {
                    continue;
                };
                let Some(choice) = choices
                    .iter()
                    .copied()
                    .find(|choice| choice.as_str() == id.as_str())
                else {
                    break PermissionMenuChoice::Cancelled;
                };
                break PermissionMenuChoice::Selected(choice);
            }
            line @ (Line::Ended | Line::ExternalBreak(_)) => {
                break PermissionMenuChoice::Propagate(line);
            }
            Line::Submitted(_)
            | Line::Blank
            | Line::Interrupted
            | Line::Refused(_)
            | Line::HostCommand(_) => break PermissionMenuChoice::Cancelled,
        }
    };
    status
        .lock()
        .expect("surface status lock")
        .question_selector = None;
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );

    let choice = match choice {
        PermissionMenuChoice::Selected(choice) => choice,
        PermissionMenuChoice::Cancelled => return None,
        PermissionMenuChoice::Propagate(line) => return Some(line),
    };
    build_permission_request(editor, phase, status, busy, choice)
}

fn build_permission_request(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
    choice: PermissionMenuId,
) -> Option<Line> {
    macro_rules! field {
        ($prompt:literal) => {
            match read_permission_field(editor, phase, status, busy, $prompt) {
                PermissionField::Value(value) => value,
                PermissionField::Cancelled => return None,
                PermissionField::Propagate(line) => return Some(line),
            }
        };
    }
    let request = match choice {
        PermissionMenuId::List => PolicyControlRequest::Permission(PermissionInput::List),
        PermissionMenuId::Allow | PermissionMenuId::Deny => {
            let raw = field!("Canonical permission rule: ");
            let rule = match managed_rule(&raw) {
                Ok(rule) => rule,
                Err(error) => return permission_failure(status, error),
            };
            PolicyControlRequest::Rule(RuleMutation {
                effect: if choice == PermissionMenuId::Allow {
                    ManagedRuleEffect::Allow
                } else {
                    ManagedRuleEffect::Deny
                },
                rule,
            })
        }
        PermissionMenuId::Revoke => {
            let raw = field!("Canonical permission rule ID: ");
            let id = match managed_rule_id(raw) {
                Ok(id) => id,
                Err(error) => return permission_failure(status, error),
            };
            PolicyControlRequest::Permission(PermissionInput::Revoke(id))
        }
        PermissionMenuId::Remember => {
            let raw_effect = field!("Rule effect (allow or deny): ");
            let effect = match managed_rule_effect(&raw_effect) {
                Ok(effect) => effect,
                Err(error) => return permission_failure(status, error),
            };
            let name = field!("Tool name: ");
            let raw_arguments = field!("Tool arguments (JSON object): ");
            let arguments = match rule_arguments(&raw_arguments) {
                Ok(arguments) => arguments,
                Err(error) => return permission_failure(status, error),
            };
            let (call, workspace) = match prepare_permission_call(status, &name, &arguments) {
                Ok(prepared) => prepared,
                Err(error) => return permission_failure(status, error),
            };
            PolicyControlRequest::Permission(PermissionInput::Remember(RememberRequest::new(
                call, effect, workspace,
            )))
        }
        PermissionMenuId::Explain => {
            let name = field!("Tool name: ");
            let raw_arguments = field!("Tool arguments (JSON object): ");
            let arguments = match rule_arguments(&raw_arguments) {
                Ok(arguments) => arguments,
                Err(error) => return permission_failure(status, error),
            };
            let (call, workspace) = match prepare_permission_call(status, &name, &arguments) {
                Ok(prepared) => prepared,
                Err(error) => return permission_failure(status, error),
            };
            PolicyControlRequest::Permission(PermissionInput::Explain { call, workspace })
        }
    };
    Some(Line::HostCommand(HostCommand::Policy(request)))
}

fn read_permission_field(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
    text: &str,
) -> PermissionField {
    let saved = enter_modal(editor, phase, status);
    let prompt = Question::new(text.to_owned(), Arc::clone(status));
    let field = match answer(false, editor.read_line(&prompt)) {
        Line::Submitted(value) => PermissionField::Value(value),
        line @ (Line::Ended | Line::ExternalBreak(_)) => PermissionField::Propagate(line),
        Line::Blank | Line::Interrupted | Line::Refused(_) | Line::HostCommand(_) => {
            PermissionField::Cancelled
        }
    };
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );
    field
}

fn prepare_permission_call(
    status: &SharedStatus,
    name: &str,
    arguments: &serde_json::Map<String, serde_json::Value>,
) -> Result<(ego_core::permit::PreparedCall, CanonPath), String> {
    let (actions, cwd) = {
        let current = status.lock().map_err(|_| "surface status is unavailable")?;
        let actions = current
            .mcp_actions
            .clone()
            .ok_or("the permission tool catalogue is unavailable")?;
        let cwd = current
            .status_base
            .as_ref()
            .map(|base| base.cwd.clone())
            .ok_or("the permission workspace is unavailable")?;
        (actions, cwd)
    };
    let workspace = OsFs
        .canonicalize(&cwd)
        .map_err(|error| format!("permission workspace could not be resolved: {error}"))?;
    let capture = actions
        .registry
        .capture()
        .map_err(|_| "the admitted permission tool catalogue is unavailable".to_owned())?;
    let mut admitted = actions
        .native_tools
        .iter()
        .filter_map(RegisteredTool::as_effect)
        .cloned()
        .collect::<Vec<Arc<dyn Tool>>>();
    admitted.extend(capture.tools().iter().cloned());
    prepare_from_catalogue(name, arguments, &workspace, &OsFs, &admitted)
        .map(|call| (call, workspace))
        .map_err(|error| error.to_string())
}

fn permission_failure(status: &SharedStatus, error: impl std::fmt::Display) -> Option<Line> {
    status.lock().expect("surface status lock").notice =
        Some(format!("permission control failed: {error}"));
    repaint_status(status);
    None
}

fn choose_policy_mode(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
) -> Option<Line> {
    let mut controller = PolicyControlController::new();
    let modes = controller.mode_options();
    let request = QuestionRequest::new(
        "Choose execution mode",
        QuestionMode::Single,
        modes
            .iter()
            .map(|mode| QuestionOption::new(mode.as_str(), format!("{mode:?}")))
            .collect(),
        false,
    );
    let selector = Arc::new(Mutex::new(Selector::with_options(
        SelectorKind::Single,
        request
            .options
            .iter()
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone())),
    )));
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(status);

    let choice = loop {
        let prompt = Question::new(
            question_selector_prompt(&request, &selector),
            Arc::clone(status),
        );
        match answer(false, editor.read_line(&prompt)) {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = question_selector_key(&command) else {
                    break PolicyModeChoice::Cancelled;
                };
                let mut selector = selector.lock().expect("policy mode selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break PolicyModeChoice::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    repaint_status(status);
                    continue;
                }
                let Some(SelectorValue::Single(id)) = selector.result() else {
                    continue;
                };
                let Some(mode) = modes
                    .iter()
                    .copied()
                    .find(|mode| mode.as_str() == id.as_str())
                else {
                    break PolicyModeChoice::Cancelled;
                };
                break PolicyModeChoice::Selected(mode);
            }
            Line::ExternalBreak(buffer) => {
                break PolicyModeChoice::Propagate(Line::ExternalBreak(buffer));
            }
            Line::Submitted(_)
            | Line::Blank
            | Line::Interrupted
            | Line::Ended
            | Line::Refused(_)
            | Line::HostCommand(_) => break PolicyModeChoice::Cancelled,
        }
    };
    status
        .lock()
        .expect("surface status lock")
        .question_selector = None;
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );

    match choice {
        PolicyModeChoice::Selected(mode) => match controller.select_mode(mode) {
            Some(request) => Some(Line::HostCommand(HostCommand::Policy(request))),
            None => confirm_yolo(editor, phase, status, busy, &mut controller),
        },
        PolicyModeChoice::Cancelled => None,
        PolicyModeChoice::Propagate(line) => Some(line),
    }
}

fn confirm_yolo(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
    controller: &mut PolicyControlController,
) -> Option<Line> {
    let saved = enter_modal(editor, phase, status);
    let prompt = Question::new(
        "Type yolo to confirm relaxed execution policy: ".to_owned(),
        Arc::clone(status),
    );
    let outcome = match answer(false, editor.read_line(&prompt)) {
        Line::Submitted(value) => controller.confirm_yolo(ModalInput::Text(value)),
        Line::Interrupted | Line::HostCommand(HostCommand::Interrupt) => {
            controller.confirm_yolo(ModalInput::CtrlC)
        }
        Line::Ended => controller.confirm_yolo(ModalInput::Eof),
        Line::ExternalBreak(buffer) => {
            let _ = controller.confirm_yolo(ModalInput::Cancel);
            leave_modal(
                editor,
                phase,
                status,
                saved,
                if busy {
                    EditPhase::Busy
                } else {
                    EditPhase::Idle
                },
            );
            return Some(Line::ExternalBreak(buffer));
        }
        Line::Blank | Line::Refused(_) | Line::HostCommand(_) => {
            controller.confirm_yolo(ModalInput::Cancel)
        }
    };
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );
    outcome.map(|request| Line::HostCommand(HostCommand::Policy(request)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Confirmation {
    Confirmed,
    Declined,
    Propagate(Line),
}

fn confirm_surface_action(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    text: &str,
    busy: bool,
) -> Confirmation {
    let saved = enter_modal(editor, phase, status);
    let prompt = Question::new(text.to_string(), Arc::clone(status));
    let answer = loop {
        match answer(false, editor.read_line(&prompt)) {
            Line::Submitted(answer) => match parse_trust(&answer) {
                Some(Trust::Trusted) => break Confirmation::Confirmed,
                Some(Trust::Untrusted) => break Confirmation::Declined,
                None => continue,
            },
            Line::HostCommand(HostCommand::ModalEscape) => {
                break Confirmation::Declined;
            }
            Line::Interrupted | Line::Blank => break Confirmation::Declined,
            line @ (Line::Ended | Line::ExternalBreak(_)) => {
                break Confirmation::Propagate(line);
            }
            Line::Refused(_) | Line::HostCommand(_) => {}
        }
    };
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );
    answer
}

enum StatuslineChoice {
    Selected(Vec<ego_core::config::StatusSegmentName>),
    Cancelled,
    Propagate(Line),
}

/// The segments each `/statusline` preset writes, and the label that offers it.
///
/// One table rather than two, because a preset whose label disagrees with what
/// it writes is a lie the operator only finds out about after saving.
const STATUSLINE_PRESETS: [(&str, &str, &[ego_core::config::StatusSegmentName]); 4] = {
    use ego_core::config::StatusSegmentName as Segment;

    [
        ("off", "Off", &[]),
        (
            "minimal",
            "Minimal — model, effort, mode",
            &[Segment::Model, Segment::Effort, Segment::Mode],
        ),
        (
            "default",
            "Default — model, effort, mode, context, queue, drafts, interrupt, cwd, git, trust",
            &[
                Segment::Model,
                Segment::Effort,
                Segment::Mode,
                Segment::Context,
                Segment::Queue,
                Segment::Drafts,
                Segment::Interrupt,
                Segment::Cwd,
                Segment::Git,
                Segment::Trust,
            ],
        ),
        (
            "full",
            "Full — model, effort, mode, context, cost, queue, drafts, interrupt, cwd, git, trust",
            &[
                Segment::Model,
                Segment::Effort,
                Segment::Mode,
                Segment::Context,
                Segment::Cost,
                Segment::Queue,
                Segment::Drafts,
                Segment::Interrupt,
                Segment::Cwd,
                Segment::Git,
                Segment::Trust,
            ],
        ),
    ]
};

fn choose_statusline_preset(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    busy: bool,
) -> StatuslineChoice {
    let request = QuestionRequest::new(
        "Choose status-line setup",
        QuestionMode::Single,
        STATUSLINE_PRESETS
            .iter()
            .map(|(id, label, _)| QuestionOption::new(*id, *label))
            .collect(),
        false,
    );
    let selector = Arc::new(Mutex::new(Selector::with_options(
        SelectorKind::Single,
        request
            .options
            .iter()
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone())),
    )));
    let saved = enter_modal(editor, phase, status);
    status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(status);

    let choice = loop {
        let prompt = Question::new(
            question_selector_prompt(&request, &selector),
            Arc::clone(status),
        );
        match answer(false, editor.read_line(&prompt)) {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = question_selector_key(&command) else {
                    continue;
                };
                let mut selector = selector.lock().expect("status-line selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break StatuslineChoice::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    repaint_status(status);
                    continue;
                }
                let Some(SelectorValue::Single(id)) = selector.result() else {
                    continue;
                };
                let Some((_, _, segments)) = STATUSLINE_PRESETS
                    .iter()
                    .find(|(preset, _, _)| *preset == id.as_str())
                else {
                    continue;
                };
                break StatuslineChoice::Selected(segments.to_vec());
            }
            Line::Interrupted | Line::Blank => break StatuslineChoice::Cancelled,
            Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                break StatuslineChoice::Cancelled;
            }
            line @ (Line::Ended | Line::ExternalBreak(_)) => {
                break StatuslineChoice::Propagate(line);
            }
            Line::Submitted(_) | Line::Refused(_) | Line::HostCommand(_) => {}
        }
    };
    status
        .lock()
        .expect("surface status lock")
        .question_selector = None;
    leave_modal(
        editor,
        phase,
        status,
        saved,
        if busy {
            EditPhase::Busy
        } else {
            EditPhase::Idle
        },
    );
    choice
}

fn configure_statusline(status: &SharedStatus, segments: Vec<ego_core::config::StatusSegmentName>) {
    let store = status
        .lock()
        .expect("surface status lock")
        .config_store
        .clone();
    let Some(store) = store else {
        status.lock().expect("surface status lock").notice =
            Some("the user-config mutation controller is not installed".to_string());
        repaint_status(status);
        return;
    };
    let names = segments
        .iter()
        .map(|segment| segment.as_str().to_string())
        .collect::<Vec<_>>();
    match store.mutate(UserConfigMutation::Statusline { segments: names }) {
        Ok(result) => {
            let mut current = status.lock().expect("surface status lock");
            current.configured = segments.into_iter().map(StatusSegmentName::from).collect();
            current.native_status = !current.configured.is_empty();
            current.notice = Some(match result.publication {
                UserConfigPublication::NotPublished if current.native_status => {
                    "status line already enabled; configuration unchanged".to_string()
                }
                UserConfigPublication::NotPublished => {
                    "status line already disabled; configuration unchanged".to_string()
                }
                UserConfigPublication::PublishedDurably if current.native_status => {
                    "status line enabled and saved".to_string()
                }
                UserConfigPublication::PublishedDurably => {
                    "status line disabled and saved".to_string()
                }
                UserConfigPublication::PublishedDurabilityUncertain { diagnostic } => {
                    format!("status line saved with uncertain durability: {diagnostic}")
                }
            });
        }
        Err(error) => {
            status.lock().expect("surface status lock").notice = Some(error.to_string());
        }
    }
    repaint_status(status);
}

fn run_model_picker(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    picker: Arc<Mutex<ModelPicker>>,
    busy: bool,
) -> Option<Line> {
    let after = if busy {
        EditPhase::Busy
    } else {
        EditPhase::Idle
    };
    let saved = enter_modal(editor, phase, status);
    let result = loop {
        let view = picker.lock().expect("model picker lock").view();
        let prompt = Question::new(model_picker_prompt(&view), Arc::clone(status));
        let line = answer(false, editor.read_line(&prompt));
        let input = match &line {
            Line::HostCommand(HostCommand::ModelPicker(command)) => model_picker_input(command),
            Line::HostCommand(_) => None,
            Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => Some(PickerInput::Escape),
            Line::Blank | Line::Submitted(_) | Line::Refused(_) => None,
        };
        let Some(input) = input else {
            continue;
        };
        let outcome = picker.lock().expect("model picker lock").input(input);
        match outcome {
            Ok(PickerInputOutcome::Closed(outcome)) => match outcome {
                PickerOutcome::Cancelled => {
                    break matches!(&line, Line::Ended | Line::ExternalBreak(_)).then_some(line);
                }
                PickerOutcome::ModelSwitch(request) => {
                    break Some(Line::HostCommand(HostCommand::ModelSwitch(request)));
                }
            },
            Ok(PickerInputOutcome::Consumed | PickerInputOutcome::FocusChanged(_)) => {
                status.lock().expect("surface status lock").notice = None;
                repaint_status(status);
            }
            Err(error) => {
                status.lock().expect("surface status lock").notice = Some(error.to_string());
                repaint_status(status);
            }
        }
    };
    leave_modal(editor, phase, status, saved, after);
    result
}

fn model_picker_input(command: &str) -> Option<PickerInput> {
    match command {
        "up" => Some(PickerInput::Up),
        "down" => Some(PickerInput::Down),
        "enter" => Some(PickerInput::Enter),
        "session" => Some(PickerInput::SessionOnly),
        "escape" => Some(PickerInput::Escape),
        number => number
            .strip_prefix("number:")?
            .parse()
            .ok()
            .map(PickerInput::Number),
    }
}

fn model_picker_prompt(view: &ModelPickerView) -> String {
    let mut rows = Vec::new();
    match view.focus {
        PickerFocus::Model => {
            rows.push("Select model".to_string());
            rows.extend(view.models.iter().enumerate().map(|(index, row)| {
                let id = row.slug.to_string();
                let marker = if view.current_id.as_deref() == Some(id.as_str()) {
                    ">"
                } else {
                    " "
                };
                let context = row
                    .max_context_tokens
                    .map(|tokens| tokens.to_string())
                    .unwrap_or_else(|| "unknown context".to_string());
                format!(
                    "{marker} {}. {} | {context} | {:?} | {:?}",
                    index + 1,
                    row.slug,
                    row.reasoning,
                    row.availability
                )
            }));
            rows.push("Up/Down or number selects; Enter continues; Esc cancels. ".to_string());
        }
        PickerFocus::Reasoning => {
            rows.push(format!(
                "Select reasoning for {}",
                view.selected_model
                    .as_deref()
                    .unwrap_or("the selected model")
            ));
            rows.extend(view.reasoning.iter().enumerate().map(|(index, reasoning)| {
                let marker = if view.current_id.as_deref() == Some(reasoning.as_str()) {
                    ">"
                } else {
                    " "
                };
                format!("{marker} {}. {reasoning:?}", index + 1)
            }));
            rows.push(
                "Up/Down or number selects; Enter saves defaults; s uses this session; Esc cancels. "
                    .to_string(),
            );
        }
    }
    rows.join("\n")
}

fn enter_modal(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
) -> SavedComposer {
    let saved = SavedComposer {
        text: editor.current_buffer_contents().to_string(),
        cursor: editor.current_insertion_point(),
        origin: status.lock().expect("surface status lock").origin,
    };
    editor.run_edit_commands(&[EditCommand::Clear]);
    phase.set(EditPhase::Modal);
    {
        let mut status = status.lock().expect("surface status lock");
        status.notice = None;
        if let Some(snapshot) = status.snapshot.as_mut() {
            snapshot.set_modal(true);
        }
    }
    repaint_status(status);
    saved
}

fn leave_modal(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    status: &SharedStatus,
    saved: SavedComposer,
    after: EditPhase,
) {
    editor.run_edit_commands(&[
        EditCommand::Clear,
        EditCommand::InsertString(saved.text),
        EditCommand::MoveToPosition {
            position: saved.cursor,
            select: false,
        },
    ]);
    {
        let mut status = status.lock().expect("surface status lock");
        status.origin = saved.origin;
        status.notice = None;
        if let Some(snapshot) = status.snapshot.as_mut() {
            snapshot.set_modal(false);
        }
    }
    phase.set(after);
    repaint_status(status);
}

fn answer_control(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    surface: &mut ModalSurface<'_>,
    controls: &Receiver<Control>,
    pending_control: &mut Option<Control>,
    control: Control,
) -> bool {
    let control = match control {
        Control::ActivateSession { target, reply } => {
            surface.drafts.activate(target);
            {
                let mut current = surface.status.lock().expect("surface status lock");
                if let Some(base) = current.status_base.as_mut() {
                    base.session = target;
                }
                current.drafts = surface.drafts.active_count();
                let draft_count = current.drafts;
                if let Some(snapshot) = current.snapshot.as_mut() {
                    snapshot.set_drafts(draft_count);
                }
                current.notice = None;
            }
            repaint_status(surface.status);
            let _ = reply.send(true);
            return true;
        }
        control => control,
    };
    let control = match control {
        Control::ConfirmRemember { reply } => {
            let busy = phase.get() == EditPhase::Busy;
            let confirmation = confirm_surface_action(
                editor,
                phase,
                surface.status,
                "Apply these remembered permission rules? [y/N] ",
                busy,
            );
            let _ = reply.send(confirmation);
            return true;
        }
        control => control,
    };
    if matches!(&control, Control::Activate { .. }) {
        return true;
    }
    let saved = enter_modal(editor, phase, surface.status);

    enum Reply {
        Compaction(Sender<bool>, bool),
        Trust(Sender<Trust>, Trust),
        Permission(oneshot::Sender<PermissionDecision>, PermissionDecision),
        Question(oneshot::Sender<QuestionOutcome>, QuestionOutcome),
    }

    let (reply, after, mut keep_running) = match control {
        Control::ConfirmCompaction { source, reply } => {
            let confirmed = if surface.drafts.active() != source {
                false
            } else {
                let drafts = surface.drafts.active_count();
                if drafts == 0 {
                    true
                } else {
                    let prompt = Question::new(
                        format!(
                            "Compact session {source} into a new clean durable session while retaining {drafts} source-owned drafts? [y/N] "
                        ),
                        Arc::clone(surface.status),
                    );
                    loop {
                        match modal_line(editor, surface, &prompt) {
                            Line::Submitted(answer) => match parse_trust(&answer) {
                                Some(Trust::Trusted) => break true,
                                Some(Trust::Untrusted) => break false,
                                None => continue,
                            },
                            Line::Interrupted
                            | Line::Blank
                            | Line::Ended
                            | Line::ExternalBreak(_)
                            | Line::HostCommand(
                                HostCommand::Interrupt | HostCommand::ModalEscape,
                            ) => break false,
                            Line::Refused(_) | Line::HostCommand(_) => continue,
                        }
                    }
                }
            };
            (Reply::Compaction(reply, confirmed), EditPhase::Idle, true)
        }
        Control::Trust { workspace, reply } => {
            let prompt = Question::new(
                format!("Trust project files in {workspace}? [y/N] "),
                Arc::clone(surface.status),
            );
            let trust = loop {
                match modal_line(editor, surface, &prompt) {
                    Line::Submitted(answer) => match parse_trust(&answer) {
                        Some(trust) => break trust,
                        None => continue,
                    },
                    Line::Blank | Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                        break Trust::Untrusted;
                    }
                    Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        break Trust::Untrusted;
                    }
                    Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            };
            (Reply::Trust(reply, trust), EditPhase::Idle, true)
        }
        Control::Permission { request, reply } => {
            let prompt = Question::new(
                format!("{request}. [allow/deny/amend] "),
                Arc::clone(surface.status),
            );
            let decision = loop {
                match modal_line(editor, surface, &prompt) {
                    Line::Submitted(answer) => match parse_permission(&answer) {
                        Some(PermissionWord::Allow) => break PermissionDecision::Allow,
                        Some(PermissionWord::Deny) => break PermissionDecision::Deny,
                        Some(PermissionWord::Amend) => {
                            break amendment(editor, surface);
                        }
                        None => continue,
                    },
                    Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                        break PermissionDecision::Deny;
                    }
                    Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                        break PermissionDecision::Deny;
                    }
                    Line::Blank | Line::Refused(_) | Line::HostCommand(_) => continue,
                }
            };
            (Reply::Permission(reply, decision), EditPhase::Busy, true)
        }
        Control::Question {
            id: _id,
            request,
            reply,
        } => {
            let outcome = answer_question(editor, surface, &request);
            (Reply::Question(reply, outcome), EditPhase::Busy, true)
        }
        Control::Shutdown => return false,
        Control::ActivateSession { .. } => unreachable!("activation is handled before modal entry"),
        Control::Activate { .. } => unreachable!("activation is handled before modal entry"),
        Control::ConfirmRemember { .. } => {
            unreachable!("remember confirmation is handled before modal entry")
        }
    };
    match controls.try_recv() {
        Ok(Control::Shutdown) => keep_running = false,
        Ok(next) => *pending_control = Some(next),
        Err(crossbeam::channel::TryRecvError::Empty) => {}
        Err(crossbeam::channel::TryRecvError::Disconnected) => keep_running = false,
    }

    leave_modal(editor, phase, surface.status, saved, after);
    match reply {
        Reply::Compaction(sender, confirmed) => {
            let _ = sender.send(confirmed);
        }
        Reply::Trust(sender, trust) => {
            let _ = sender.send(trust);
        }
        Reply::Permission(sender, decision) => {
            let _ = sender.send(decision);
        }
        Reply::Question(sender, outcome) => {
            let _ = sender.send(outcome);
        }
    }
    keep_running
}

fn answer_question(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    request: &QuestionRequest,
) -> QuestionOutcome {
    let cursor_selectors = surface
        .status
        .lock()
        .expect("surface status lock")
        .cursor_selectors;
    if cursor_selectors && matches!(request.mode, QuestionMode::Single | QuestionMode::Multiple) {
        return answer_question_selector(editor, surface, request);
    }
    let prompt = Question::new(question_prompt(request), Arc::clone(surface.status));
    loop {
        match modal_line(editor, surface, &prompt) {
            Line::Submitted(answer) => match parse_question_answer(request, &answer) {
                Ok(answer) => return QuestionOutcome::Answered(answer),
                Err(error) => {
                    surface.status.lock().expect("surface status lock").notice =
                        Some(error.to_string());
                }
            },
            Line::HostCommand(HostCommand::ModalEscape) => {
                return QuestionOutcome::Cancelled;
            }
            Line::Interrupted | Line::Blank => return QuestionOutcome::Declined,
            Line::Ended | Line::ExternalBreak(_) => return QuestionOutcome::Unavailable,
            Line::Refused(_) | Line::HostCommand(_) => {}
        }
    }
}

fn answer_question_selector(
    editor: &mut Reedline,
    surface: &mut ModalSurface<'_>,
    request: &QuestionRequest,
) -> QuestionOutcome {
    let kind = match request.mode {
        QuestionMode::Single => SelectorKind::Single,
        QuestionMode::Multiple => SelectorKind::Multiple,
        QuestionMode::FreeText => unreachable!("free text retains the editor grammar"),
        QuestionMode::Form { .. } => {
            unreachable!("form answers retain the raw JSON editor grammar")
        }
    };
    let selector = Arc::new(Mutex::new(Selector::with_options(
        kind,
        request
            .options
            .iter()
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone())),
    )));
    surface
        .status
        .lock()
        .expect("surface status lock")
        .question_selector = Some(Arc::clone(&selector));
    repaint_status(surface.status);

    let outcome = loop {
        let prompt = Question::new(
            question_selector_prompt(request, &selector),
            Arc::clone(surface.status),
        );
        match modal_line(editor, surface, &prompt) {
            Line::HostCommand(HostCommand::QuestionSelector(command)) => {
                let Some(key) = question_selector_key(&command) else {
                    continue;
                };
                let mut selector = selector.lock().expect("question selector lock");
                selector.key(key);
                if key == SelectorKey::Escape {
                    break QuestionOutcome::Cancelled;
                }
                if key != SelectorKey::Enter {
                    drop(selector);
                    surface.status.lock().expect("surface status lock").notice = None;
                    repaint_status(surface.status);
                    continue;
                }
                let answer = match selector.result() {
                    Some(SelectorValue::Single(id)) => QuestionAnswer::single(id.clone()),
                    Some(SelectorValue::Multiple(ids)) => QuestionAnswer::multiple(ids.clone()),
                    Some(SelectorValue::FreeText(_)) | None => continue,
                };
                drop(selector);
                match answer.validate_for(request) {
                    Ok(()) => break QuestionOutcome::Answered(answer),
                    Err(error) => {
                        surface.status.lock().expect("surface status lock").notice =
                            Some(error.to_string());
                        repaint_status(surface.status);
                    }
                }
            }
            Line::Interrupted | Line::Blank => break QuestionOutcome::Declined,
            Line::Ended | Line::ExternalBreak(_) => break QuestionOutcome::Unavailable,
            Line::Submitted(_) | Line::Refused(_) | Line::HostCommand(_) => {}
        }
    };
    surface
        .status
        .lock()
        .expect("surface status lock")
        .question_selector = None;
    repaint_status(surface.status);
    outcome
}

fn question_selector_key(command: &str) -> Option<SelectorKey> {
    match command {
        "up" => Some(SelectorKey::Up),
        "down" => Some(SelectorKey::Down),
        "space" => Some(SelectorKey::Space),
        "enter" => Some(SelectorKey::Enter),
        "escape" => Some(SelectorKey::Escape),
        number => number
            .strip_prefix("number:")?
            .parse()
            .ok()
            .map(SelectorKey::Number),
    }
}

fn question_selector_prompt(request: &QuestionRequest, selector: &Arc<Mutex<Selector>>) -> String {
    let selector = selector.lock().expect("question selector lock");
    let current = selector.current_id();
    let selected = selector.selected_ids();
    let mut rows = vec![request.prompt.clone()];
    rows.extend(request.options.iter().enumerate().map(|(index, option)| {
        let marker = if current == Some(option.id.as_str()) {
            ">"
        } else {
            " "
        };
        if request.mode == QuestionMode::Multiple {
            let checked = if selected.contains(&option.id.as_str()) {
                "x"
            } else {
                " "
            };
            format!("{marker} [{checked}] {}. {}", index + 1, option.label)
        } else {
            format!("{marker} {}. {}", index + 1, option.label)
        }
    }));
    rows.push(if request.mode == QuestionMode::Multiple {
        "Up/Down moves; Space or number toggles; Enter answers; Esc cancels. ".to_string()
    } else {
        "Up/Down or number selects; Enter answers; Esc cancels. ".to_string()
    });
    rows.join("\n")
}

fn question_prompt(request: &QuestionRequest) -> String {
    let options = request
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| format!("{}. {}", index + 1, option.label))
        .collect::<Vec<_>>()
        .join("; ");
    if options.is_empty() {
        format!("{} ", request.prompt)
    } else {
        format!("{} [{}] ", request.prompt, options)
    }
}

fn parse_question_answer(
    request: &QuestionRequest,
    raw: &str,
) -> Result<QuestionAnswer, ego_core::question::QuestionError> {
    let options = || {
        request
            .options
            .iter()
            .map(|option| SelectorOption::new(option.id.clone(), option.label.clone()))
    };
    let option_number = |value: &str| {
        value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|number| *number > 0 && *number <= request.options.len())
            .or_else(|| {
                request
                    .options
                    .iter()
                    .position(|option| option.id == value.trim())
                    .map(|index| index + 1)
            })
    };
    let answer = match request.mode {
        QuestionMode::Single => {
            let mut selector = Selector::with_options(SelectorKind::Single, options());
            if let Some(number) = option_number(raw) {
                selector.key(SelectorKey::Number(number));
                selector.key(SelectorKey::Enter);
                match selector.result() {
                    Some(SelectorValue::Single(id)) => QuestionAnswer::single(id.clone()),
                    _ => QuestionAnswer::single(raw),
                }
            } else {
                QuestionAnswer::single(raw)
            }
        }
        QuestionMode::Multiple => {
            let mut selector = Selector::with_options(SelectorKind::Multiple, options());
            let mut free_text = Vec::new();
            for value in raw
                .split([',', ' '])
                .filter(|value| !value.trim().is_empty())
            {
                if let Some(number) = option_number(value) {
                    selector.key(SelectorKey::Number(number));
                } else {
                    free_text.push(value.trim().to_string());
                }
            }
            selector.key(SelectorKey::Enter);
            let mut selected = match selector.result() {
                Some(SelectorValue::Multiple(ids)) => ids.clone(),
                _ => Vec::new(),
            };
            selected.extend(free_text);
            QuestionAnswer::multiple(selected)
        }
        QuestionMode::FreeText => {
            let mut selector = Selector::free_text();
            selector.insert(raw);
            selector.key(SelectorKey::Enter);
            match selector.result() {
                Some(SelectorValue::FreeText(value)) => QuestionAnswer::free_text(value.clone()),
                _ => QuestionAnswer::free_text(raw),
            }
        }
        QuestionMode::Form { .. } => return QuestionAnswer::from_json(request, raw),
    };
    answer.validate_for(request)?;
    Ok(answer)
}

fn modal_line(editor: &mut Reedline, surface: &mut ModalSurface<'_>, prompt: &dyn Prompt) -> Line {
    surface_line(
        editor,
        prompt,
        surface.phase,
        surface.status,
        surface.asked_for_image,
        surface.ingress,
        surface.drafts,
    )
    .0
}

fn amendment(editor: &mut Reedline, surface: &mut ModalSurface<'_>) -> PermissionDecision {
    let prompt = Question::new(
        "New JSON parameters: ".to_string(),
        Arc::clone(surface.status),
    );
    loop {
        match modal_line(editor, surface, &prompt) {
            Line::Submitted(raw) => match serde_json::from_str(&raw) {
                Ok(params) => return PermissionDecision::Amend { params },
                Err(_) => continue,
            },
            Line::Interrupted | Line::Ended | Line::ExternalBreak(_) => {
                return PermissionDecision::Deny;
            }
            Line::HostCommand(HostCommand::Interrupt | HostCommand::ModalEscape) => {
                return PermissionDecision::Deny;
            }
            Line::Blank | Line::Refused(_) | Line::HostCommand(_) => continue,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionWord {
    Allow,
    Deny,
    Amend,
}

fn parse_trust(answer: &str) -> Option<Trust> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Some(Trust::Trusted),
        "n" | "no" => Some(Trust::Untrusted),
        _ => None,
    }
}

fn parse_permission(answer: &str) -> Option<PermissionWord> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "a" | "allow" => Some(PermissionWord::Allow),
        "d" | "deny" => Some(PermissionWord::Deny),
        "m" | "amend" => Some(PermissionWord::Amend),
        _ => None,
    }
}

/// The prompt: two characters and nothing else.
///
/// No model name, no token count, no cwd. Everything a status line could say
/// is either already on the screen or answerable by asking, and a prompt that
/// redraws a changing right-hand side is a second writer competing with the
/// renderer for the same row.
struct Waiting {
    status: SharedStatus,
}

impl Waiting {
    fn new(status: SharedStatus) -> Self {
        Self { status }
    }
}

fn status_prefix(status: &SharedStatus) -> String {
    let status = status.lock().expect("surface status lock");
    let mut rows = Vec::new();
    if !status.native_status {
        if status.queue.pending_steers > 0 {
            rows.push(QUEUE_LABEL.to_string());
        }
        if status.queue.interrupt_pending {
            rows.push("interrupt pending".to_string());
        }
    }
    if let Some(notice) = status.notice.as_ref() {
        rows.push(notice.clone());
    }
    if rows.is_empty() {
        String::new()
    } else {
        format!("{} | ", rows.join(" | "))
    }
}

fn status_right(status: &SharedStatus) -> String {
    let painter = Arc::clone(&status.lock().expect("surface status lock").painter);
    painter
        .current_status()
        .map(|line| line.text().to_string())
        .unwrap_or_default()
}

impl Prompt for Waiting {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(status_right(&self.status))
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Owned(format!("{}> ", status_prefix(&self.status)))
    }

    /// Alt+Enter and a bracketed paste both put a real newline in the buffer,
    /// and this is the only mark that the line being typed is still one line.
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}

/// Kept wholly in the indicator because Reedline's large-buffer path measures
/// wrapped prompt rows but skips `prompt_str_left` by logical newlines. A long
/// one-line question can therefore be measured off-screen and then sliced to
/// empty. The indicator uses the post-prompt offset and preserves the complete
/// canonical request and answer vocabulary at narrow widths.
struct Question {
    text: String,
    status: SharedStatus,
}

impl Question {
    fn new(text: String, status: SharedStatus) -> Self {
        Self { text, status }
    }
}

impl Prompt for Question {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Owned(status_right(&self.status))
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Owned(format!("{}{}", status_prefix(&self.status), self.text))
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("… ")
    }

    fn render_prompt_history_search_indicator(&self, search: PromptHistorySearch) -> Cow<'_, str> {
        Cow::Owned(format!("(search: {}) ", search.term))
    }
}

/// The Emacs keymap with ego's phase and clipboard decisions taken first.
pub struct SurfaceEditMode {
    editor: Emacs,
    phase: SurfacePhase,
    status: SharedStatus,
    asked_for_image: Arc<AtomicBool>,
}

impl SurfaceEditMode {
    pub fn new(editor: Emacs, phase: SurfacePhase) -> Self {
        Self::with_status(
            editor,
            phase,
            Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::new(
                ActionRegistry::final_surface(),
            )))),
        )
    }

    fn with_status(editor: Emacs, phase: SurfacePhase, status: SharedStatus) -> Self {
        Self {
            editor,
            phase,
            status,
            asked_for_image: Arc::new(AtomicBool::new(false)),
        }
    }

    fn asked_for_image(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.asked_for_image)
    }
}

impl EditMode for SurfaceEditMode {
    fn parse_event(&mut self, event: ReedlineRawEvent) -> ReedlineEvent {
        let raw: Event = event.into();

        let picker_open = if self.phase.get() == EditPhase::Modal {
            let picker = self
                .status
                .lock()
                .expect("surface status lock")
                .model_picker
                .clone();
            picker.is_some_and(|picker| picker.lock().expect("model picker lock").is_open())
        } else {
            false
        };
        let question_selector_open = if self.phase.get() == EditPhase::Modal {
            self.status
                .lock()
                .expect("surface status lock")
                .question_selector
                .is_some()
        } else {
            false
        };

        match &raw {
            Event::Paste(_) if !picker_open && !question_selector_open => {
                self.status.lock().expect("surface status lock").origin =
                    InputOrigin::BracketedPaste;
            }
            Event::Resize(width, _) => {
                self.status.lock().expect("surface status lock").width = usize::from(*width);
                repaint_status(&self.status);
            }
            _ => {}
        }

        if picker_open {
            return match &raw {
                Event::Key(key) if key.modifiers.is_empty() => match key.code {
                    KeyCode::Up => picker_command("up"),
                    KeyCode::Down => picker_command("down"),
                    KeyCode::Enter => picker_command("enter"),
                    KeyCode::Esc => picker_command("escape"),
                    KeyCode::Char('s') => picker_command("session"),
                    KeyCode::Char(digit @ '1'..='9') => picker_command(&format!("number:{digit}")),
                    _ => ReedlineEvent::None,
                },
                Event::Key(key)
                    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL =>
                {
                    picker_command("escape")
                }
                Event::Resize(_, _) => ReedlineEvent::None,
                _ => ReedlineEvent::None,
            };
        }

        if question_selector_open {
            return match &raw {
                Event::Key(key) if key.modifiers.is_empty() => match key.code {
                    KeyCode::Up => question_selector_command("up"),
                    KeyCode::Down => question_selector_command("down"),
                    KeyCode::Enter => question_selector_command("enter"),
                    KeyCode::Esc => question_selector_command("escape"),
                    KeyCode::Char(' ') => question_selector_command("space"),
                    KeyCode::Char(digit @ '1'..='9') => {
                        question_selector_command(&format!("number:{digit}"))
                    }
                    _ => ReedlineEvent::None,
                },
                Event::Key(key)
                    if key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL =>
                {
                    question_selector_command("escape")
                }
                Event::Resize(_, _) => ReedlineEvent::None,
                _ => ReedlineEvent::None,
            };
        }

        if let Event::Key(key) = &raw {
            if self.phase.get() == EditPhase::Modal
                && key.code == reedline::KeyCode::Esc
                && key.modifiers.is_empty()
            {
                return ReedlineEvent::ExecuteHostCommand(MODAL_ESCAPE_COMMAND.to_string());
            }
            if self.phase.get() == EditPhase::Modal {
                return match ReedlineRawEvent::try_from(raw) {
                    Ok(event) => self.editor.parse_event(event),
                    Err(()) => ReedlineEvent::None,
                };
            }
            let action = self
                .status
                .lock()
                .expect("surface status lock")
                .actions
                .as_ref()
                .and_then(|actions| actions.registry.resolve_key(*key));
            if let Some(action) = action {
                let name = self
                    .status
                    .lock()
                    .expect("surface status lock")
                    .actions
                    .as_ref()
                    .map(|actions| actions.registry.help(action).name().to_string())
                    .unwrap_or_default();
                return ReedlineEvent::ExecuteHostCommand(format!("{ACTION_COMMAND}{name}"));
            }
            match intercept(key.code, key.modifiers) {
                Intercept::ClipboardImage => {
                    self.asked_for_image.store(true, Ordering::Relaxed);
                    return ReedlineEvent::ExecuteHostCommand(String::new());
                }
                Intercept::Draft => {
                    return ReedlineEvent::ExecuteHostCommand(DRAFT_COMMAND.to_string());
                }
                Intercept::SubmitBusy if self.phase.get() == EditPhase::Busy => {
                    return ReedlineEvent::ExecuteHostCommand(BUSY_SUBMIT_COMMAND.to_string());
                }
                Intercept::Interrupt
                    if self
                        .status
                        .lock()
                        .expect("surface status lock")
                        .queue
                        .pending_steers
                        > 0 =>
                {
                    return ReedlineEvent::ExecuteHostCommand(INTERRUPT_COMMAND.to_string());
                }
                Intercept::Editor | Intercept::SubmitBusy | Intercept::Interrupt => {}
            }
        }

        match ReedlineRawEvent::try_from(raw) {
            Ok(event) => self.editor.parse_event(event),
            // Unreachable in practice: the only rejected event is a key
            // release, and `raw` came out of a `ReedlineRawEvent`, which is
            // the type that already excluded one.
            Err(()) => ReedlineEvent::None,
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.editor.edit_mode()
    }
}

fn picker_command(command: &str) -> ReedlineEvent {
    ReedlineEvent::ExecuteHostCommand(format!("{MODEL_PICKER_COMMAND}{command}"))
}

fn question_selector_command(command: &str) -> ReedlineEvent {
    ReedlineEvent::ExecuteHostCommand(format!("{QUESTION_SELECTOR_COMMAND}{command}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repl::policy_controls::PolicyControlRequest;

    #[test]
    fn trust_accepts_only_an_explicit_yes_or_no() {
        for yes in ["y", "yes", " YES "] {
            assert_eq!(parse_trust(yes), Some(Trust::Trusted));
        }
        for no in ["n", "no", " NO "] {
            assert_eq!(parse_trust(no), Some(Trust::Untrusted));
        }
        for invalid in ["", "maybe", "allow", "1"] {
            assert_eq!(parse_trust(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn every_populated_statusline_preset_states_the_workspace_posture() {
        use ego_core::config::StatusSegmentName as Segment;

        for (id, label, segments) in STATUSLINE_PRESETS {
            let named = segments
                .iter()
                .map(|segment| segment.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            assert!(
                label == "Off" || label.ends_with(&named),
                "the `{id}` label promises something other than what it writes: {label}"
            );
        }

        for preset in ["default", "full"] {
            let (_, _, segments) = STATUSLINE_PRESETS
                .iter()
                .find(|(id, _, _)| *id == preset)
                .expect("a preset the picker offers");
            assert!(
                segments.contains(&Segment::Trust),
                "a session that starts untrusted in silence needs `{preset}` to say so"
            );
        }
    }

    #[test]
    fn the_plain_permission_vocabulary_includes_amend() {
        for (raw, expected) in [
            ("allow", PermissionWord::Allow),
            ("deny", PermissionWord::Deny),
            ("amend", PermissionWord::Amend),
            (" A ", PermissionWord::Allow),
            ("d", PermissionWord::Deny),
            ("m", PermissionWord::Amend),
        ] {
            assert_eq!(parse_permission(raw), Some(expected), "{raw:?}");
        }
        for invalid in ["", "yes", "no", "approve", "cancel"] {
            assert_eq!(parse_permission(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn long_queue_status_uses_the_unsliced_indicator_path() {
        let status = Arc::new(Mutex::new(SurfaceStatus {
            queue: QueueStatus {
                pending_steers: 1,
                queued: 0,
                interrupt_pending: false,
            },
            drafts: 0,
            notice: None,
            ..SurfaceStatus::default()
        }));
        let waiting = Waiting::new(status);

        assert_eq!(waiting.render_prompt_left().as_ref(), "");
        let indicator = waiting.render_prompt_indicator(PromptEditMode::Emacs);
        assert_eq!(indicator.as_ref(), format!("{QUEUE_LABEL} | > ").as_str());
    }

    #[test]
    fn reading_a_prebuffered_submission_settles_busy_after_idle() {
        let phase = SurfacePhase::new(EditPhase::Idle);
        let (input_sender, input) = bounded(CHANNEL_CAPACITY);
        input_sender
            .send(Line::Submitted("initial".to_string()))
            .unwrap();
        let (seat_control, _seat_control_receiver) = bounded(CHANNEL_CAPACITY);
        let (_ingress, coordinator) = interactive_control();
        let registry = Arc::new(ActionRegistry::final_surface());
        let seat = Seat {
            kind: SeatKind::Live(LiveSeat {
                input,
                control: seat_control,
                activated: AtomicBool::new(false),
                break_signal: Arc::new(AtomicBool::new(false)),
                phase: phase.clone(),
                status: Arc::new(Mutex::new(SurfaceStatus::with_registry(Arc::clone(
                    &registry,
                )))),
                git_worker: Mutex::new(None),
                owner: Mutex::new(None),
            }),
            terminal: true,
            registry,
            coordinator: Mutex::new(Some(coordinator)),
        };

        assert_eq!(seat.read(), Line::Submitted("initial".to_string()));
        assert_eq!(phase.get(), EditPhase::Busy);
    }

    #[test]
    fn enabling_policy_controls_without_catalogue_leaves_permissions_disabled() {
        let (seat, _) = Seat::new(CancelToken::new()).expect("test seat");
        let registry = seat.action_registry();
        for action in [
            ActionId::Mode,
            ActionId::Plan,
            ActionId::Yolo,
            ActionId::Permissions,
        ] {
            assert!(matches!(
                registry.availability(action),
                Availability::Disabled { .. }
            ));
        }
        seat.enable_policy_controls();
        for action in [ActionId::Mode, ActionId::Plan, ActionId::Yolo] {
            assert_eq!(registry.availability(action), Availability::Enabled);
        }
        assert!(matches!(
            registry.availability(ActionId::Permissions),
            Availability::Disabled { .. }
        ));
        seat.join();
    }

    #[test]
    fn enter_and_leave_modal_restore_composer_cursor_origin_and_phase() {
        let mut editor = Reedline::create();
        editor.run_edit_commands(&[
            EditCommand::InsertString("draft text".to_owned()),
            EditCommand::MoveToPosition {
                position: 5,
                select: false,
            },
        ]);
        let phase = SurfacePhase::new(EditPhase::Busy);
        let status = Arc::new(Mutex::new(SurfaceStatus {
            origin: InputOrigin::BracketedPaste,
            ..SurfaceStatus::default()
        }));
        let saved = enter_modal(&mut editor, &phase, &status);
        assert_eq!(phase.get(), EditPhase::Modal);
        assert_eq!(editor.current_buffer_contents(), "");
        leave_modal(&mut editor, &phase, &status, saved, EditPhase::Busy);
        assert_eq!(editor.current_buffer_contents(), "draft text");
        assert_eq!(editor.current_insertion_point(), 5);
        assert_eq!(status.lock().unwrap().origin, InputOrigin::BracketedPaste);
        assert_eq!(phase.get(), EditPhase::Busy);
    }

    #[test]
    fn surface_action_sink_routes_all_four_policy_controls() {
        let pending = Arc::new(Mutex::new(None));
        let mut sink = SurfaceActionSink {
            pending: Arc::clone(&pending),
        };
        for (action, effect) in [
            (ActionId::Mode, SurfaceActionEffect::Mode),
            (ActionId::Plan, SurfaceActionEffect::Plan),
            (ActionId::Yolo, SurfaceActionEffect::Yolo),
            (ActionId::Permissions, SurfaceActionEffect::Permissions),
        ] {
            assert_eq!(
                sink.invoke(LocalAction {
                    id: action,
                    busy: false
                }),
                LocalActionOutcome::Completed
            );
            let observed = pending.lock().unwrap();
            assert_eq!(
                observed.as_ref().map(std::mem::discriminant),
                Some(std::mem::discriminant(&effect))
            );
            drop(observed);
            pending.lock().unwrap().take();
        }
    }

    #[test]
    fn regression_case_surface_sink_routes_name_and_goal() {
        let pending = Arc::new(Mutex::new(None));
        let mut sink = SurfaceActionSink {
            pending: Arc::clone(&pending),
        };
        for (id, effect) in [
            (ActionId::Name, SurfaceActionEffect::Name),
            (ActionId::Goal, SurfaceActionEffect::Goal),
        ] {
            assert_eq!(
                sink.invoke(LocalAction { id, busy: false }),
                LocalActionOutcome::Completed
            );
            assert_eq!(
                pending.lock().unwrap().as_ref().map(std::mem::discriminant),
                Some(std::mem::discriminant(&effect))
            );
            pending.lock().unwrap().take();
        }
    }

    #[test]
    fn regression_case_surface_effect_maps_idle_and_refuses_busy() {
        let mut editor = Reedline::create();
        let phase = SurfacePhase::new(EditPhase::Idle);
        let status = Arc::new(Mutex::new(SurfaceStatus::default()));
        assert!(matches!(
            apply_surface_effect(
                &mut editor,
                &phase,
                &status,
                SurfaceActionEffect::Name,
                false
            ),
            Some(Line::HostCommand(HostCommand::SessionMetadata(
                crate::cli::SessionMetadataOperation::Name(crate::cli::NameOperation::Show)
            )))
        ));
        assert!(matches!(
            apply_surface_effect(
                &mut editor,
                &phase,
                &status,
                SurfaceActionEffect::Goal,
                false
            ),
            Some(Line::HostCommand(HostCommand::SessionMetadata(
                crate::cli::SessionMetadataOperation::Goal(crate::cli::GoalOperation::Show)
            )))
        ));
        for effect in [SurfaceActionEffect::Name, SurfaceActionEffect::Goal] {
            status.lock().unwrap().notice = None;
            assert!(apply_surface_effect(&mut editor, &phase, &status, effect, true).is_none());
            assert!(
                status
                    .lock()
                    .unwrap()
                    .notice
                    .as_deref()
                    .unwrap_or_default()
                    .contains("unavailable while a turn is running")
            );
        }
    }

    #[test]
    fn permissions_menu_uses_six_private_ids_and_operator_spellings() {
        assert_eq!(
            permission_menu(),
            [
                PermissionMenuId::List,
                PermissionMenuId::Allow,
                PermissionMenuId::Deny,
                PermissionMenuId::Revoke,
                PermissionMenuId::Remember,
                PermissionMenuId::Explain,
            ]
        );
        assert_eq!(
            permission_menu()
                .iter()
                .map(PermissionMenuId::as_str)
                .collect::<Vec<_>>(),
            ["list", "allow", "deny", "revoke", "remember", "explain"]
        );
    }

    #[test]
    fn plan_surface_effect_emits_typed_policy_command_without_prompt() {
        let mut editor = Reedline::create();
        let phase = SurfacePhase::new(EditPhase::Idle);
        let status = Arc::new(Mutex::new(SurfaceStatus::default()));
        let line = apply_surface_effect(
            &mut editor,
            &phase,
            &status,
            SurfaceActionEffect::Plan,
            false,
        )
        .expect("plan action emits a host command");
        assert_eq!(
            line,
            Line::HostCommand(HostCommand::Policy(PolicyControlRequest::SetMode(
                Mode::Plan
            )))
        );
        assert_eq!(editor.current_buffer_contents(), "");
        assert_eq!(phase.get(), EditPhase::Idle);
    }

    #[test]
    fn source_target_source_activation_restores_source_drafts_in_lifo_order() {
        let source = SessionId::new();
        let target = SessionId::new();
        let mut drafts = DraftStash::new(source);
        drafts.push_active(ComposerDraft {
            text: "source-first".to_string(),
            cursor: 12,
            origin: InputOrigin::Typed,
        });
        drafts.push_active(ComposerDraft {
            text: "source-second".to_string(),
            cursor: 13,
            origin: InputOrigin::Typed,
        });

        drafts.activate(target);
        assert_eq!(drafts.active(), target);
        assert_eq!(drafts.active_count(), 0);
        assert_eq!(drafts.count(source), 2);
        assert_eq!(drafts.count(target), 0);
        drafts.push_active(ComposerDraft {
            text: "target-only".to_string(),
            cursor: 11,
            origin: InputOrigin::Typed,
        });
        assert_eq!(drafts.pop_active().unwrap().text, "target-only");

        drafts.activate(source);
        assert_eq!(drafts.active(), source);
        assert_eq!(drafts.active_count(), 2);
        assert_eq!(drafts.pop_active().unwrap().text, "source-second");
        assert_eq!(drafts.pop_active().unwrap().text, "source-first");
        assert_eq!(drafts.active_count(), 0);
    }
}
