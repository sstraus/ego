//! The seat: one lifetime owner for the interactive terminal.

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam::channel::{Receiver, SendError, Sender, bounded};
use crossterm::event::Event;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use reedline::{
    ColumnarMenu, Completer, CompletionResult, EditCommand, EditMode, Emacs, ExternalPrinter,
    KeyCode, KeyModifiers, Menu, MenuBuilder, MenuEvent, Painter, Prompt, PromptEditMode,
    PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, ReedlineRawEvent, Span, Suggestion,
    default_emacs_keybindings,
};

use super::actions::{
    ActionDispatch, ActionId, ActionRegistry, ActionSink, Availability, CompositeActionSink,
    CompositeActionSinkError, Dispatch, InputOrigin, LocalAction, LocalActionOutcome,
    SharedActionSink, SkillSurfaceAction,
};
use super::keys::{Intercept, intercept};
use super::policy_controls::{
    PermissionInput, PermissionRuleApplication, PolicyControlController, PolicyControlRequest,
};
use super::selector::{Selector, SelectorKey, SelectorKind, SelectorOption, SelectorValue};
use super::status::{
    GitFacts, GitFactsSource, SeatFacts, SelectionFacts, StatusLine, StatusSegmentName,
    SurfaceSnapshot,
};
use super::surface::InteractivePainter;
use super::{
    BUSY_SUBMIT_COMMAND, DRAFT_COMMAND, HostCommand, INTERRUPT_COMMAND, Line, PASTE_PATH_COMMAND,
    REMOVE_PASTE_IMAGE_COMMAND, answer,
};
use crate::cli::{managed_rule, managed_rule_effect, managed_rule_id};
use crate::model::{GenerationSelection, GenerationSelectionSnapshot};
use crate::model_picker::{
    ModelPicker, ModelPickerView, PickerFocus, PickerInput, PickerInputOutcome, PickerOutcome,
};
use crate::render::osc::Osc;
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
pub(super) const PERMISSION_DECISION_COMMAND: &str = "ego:permission-decision:";
pub(super) const MODAL_ESCAPE_COMMAND: &str = "ego:modal-escape";
const QUEUE_LABEL: &str =
    "Messages to be submitted after next tool call (press esc to interrupt and send immediately)";
const ACTIVITY_FRAMES: [&str; 4] = ["◐", "◓", "◑", "◒"];
const DOUBLE_CTRL_C_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

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
    Expand,
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
            ActionId::Expand => SurfaceActionEffect::Expand,
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
        if position == line.len() && self.registry.resolve_slash(filter).is_some() {
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

/// Reedline 0.44 keeps a completion menu active after its values become empty.
/// For an exact action that makes the next Enter accept an empty menu instead
/// of submitting the command. This adapter preserves ColumnarMenu behavior and
/// closes it only when the authoritative completer has no remaining choice.
struct ActionMenu {
    inner: ColumnarMenu,
}

impl ActionMenu {
    fn new(inner: ColumnarMenu) -> Self {
        Self { inner }
    }
}

impl Menu for ActionMenu {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn indicator(&self) -> &str {
        self.inner.indicator()
    }

    fn is_active(&self) -> bool {
        self.inner.is_active()
    }

    fn set_active(&mut self, active: bool) {
        self.inner.set_active(active);
    }

    fn clear_input(&mut self) {
        self.inner.clear_input();
    }

    fn reset_position(&mut self) {
        self.inner.reset_position();
    }

    fn menu_event(&mut self, event: MenuEvent) {
        self.inner.menu_event(event);
    }

    fn can_quick_complete(&self) -> bool {
        self.inner.can_quick_complete()
    }

    fn can_partially_complete(
        &mut self,
        values_updated: bool,
        editor: &mut reedline::Editor,
        completer: &mut dyn Completer,
    ) -> bool {
        self.inner
            .can_partially_complete(values_updated, editor, completer)
    }

    fn update_values(&mut self, editor: &mut reedline::Editor, completer: &mut dyn Completer) {
        self.inner.update_values(editor, completer);
        if self.inner.get_values().is_empty() {
            self.inner.menu_event(MenuEvent::Deactivate);
        }
    }

    fn update_working_details(
        &mut self,
        editor: &mut reedline::Editor,
        completer: &mut dyn Completer,
        painter: &Painter,
    ) {
        self.inner
            .update_working_details(editor, completer, painter);
    }

    fn replace_in_buffer(&self, editor: &mut reedline::Editor) {
        self.inner.replace_in_buffer(editor);
    }

    fn menu_required_lines(&self, terminal_columns: u16) -> u16 {
        self.inner.menu_required_lines(terminal_columns)
    }

    fn menu_string(&self, available_lines: u16, use_ansi_coloring: bool) -> String {
        self.inner.menu_string(available_lines, use_ansi_coloring)
    }

    fn min_rows(&self) -> u16 {
        self.inner.min_rows()
    }

    fn get_values(&self) -> &[Suggestion] {
        self.inner.get_values()
    }

    fn set_cursor_pos(&mut self, cursor: (u16, u16)) {
        self.inner.set_cursor_pos(cursor);
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
    activity: bool,
    activity_since: Option<Instant>,
    /// When the open modal began; the wait is the person's, not the turn's.
    modal_since: Option<Instant>,
    /// The part of the current turn spent waiting on modals.
    modal_waited: std::time::Duration,
    reply_started: Arc<AtomicBool>,
    notice: Option<String>,
    image_chips: Vec<String>,
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
    /// Keys the picker has been sent and not yet applied. The editor parses a
    /// whole burst of input before running its first command and drops the
    /// rest, so typed keys wait here instead of riding on that command.
    picker_keys: Vec<String>,
    model_selection: Option<Arc<GenerationSelection>>,
    question_selector: Option<Arc<Mutex<Selector>>>,
    permission_modal: bool,
    permission_rules: Option<Arc<dyn PermissionRuleApplication>>,
    /// Rows committed straight to the transcript by the terminal owner, for
    /// receipts that have no run output to travel with. `None` without a live
    /// terminal.
    transcript: Option<Sender<String>>,
    /// What the host is told around a modal wait, and whether `-q` is on.
    signals: Option<(Osc, bool)>,
    cursor_selectors: bool,
    mcp_actions: Option<McpSurfaceActions>,
    surface_effect: Arc<Mutex<Option<SurfaceActionEffect>>>,
}

impl SurfaceStatus {
    fn begin_activity(&mut self) {
        if !self.activity {
            self.activity_since = Some(Instant::now());
            self.modal_waited = std::time::Duration::ZERO;
            self.reply_started.store(false, Ordering::Release);
        }
        self.activity = true;
    }
}

impl Default for SurfaceStatus {
    fn default() -> Self {
        Self {
            queue: QueueStatus::default(),
            drafts: 0,
            activity: false,
            activity_since: None,
            modal_since: None,
            modal_waited: std::time::Duration::ZERO,
            reply_started: Arc::new(AtomicBool::new(false)),
            notice: None,
            image_chips: Vec::new(),
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
            picker_keys: Vec::new(),
            model_selection: None,
            question_selector: None,
            permission_modal: false,
            permission_rules: None,
            transcript: None,
            signals: None,
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
    startup_output: Receiver<String>,
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

struct ActivityWorker {
    stop: Sender<()>,
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

pub(crate) const DEFAULT_STATUSLINE_SEGMENTS: &[ego_core::config::StatusSegmentName] = &[
    ego_core::config::StatusSegmentName::Model,
    ego_core::config::StatusSegmentName::Effort,
    ego_core::config::StatusSegmentName::Mode,
    ego_core::config::StatusSegmentName::Context,
    ego_core::config::StatusSegmentName::Cost,
    ego_core::config::StatusSegmentName::Queue,
    ego_core::config::StatusSegmentName::Drafts,
    ego_core::config::StatusSegmentName::Interrupt,
    ego_core::config::StatusSegmentName::Cwd,
    ego_core::config::StatusSegmentName::Git,
    ego_core::config::StatusSegmentName::Trust,
];

pub(crate) fn effective_statusline_segments(
    configured: Option<&ego_core::config::StatuslineLayer>,
) -> Vec<ego_core::config::StatusSegmentName> {
    configured
        .and_then(|statusline| statusline.segments.clone())
        .unwrap_or_else(|| DEFAULT_STATUSLINE_SEGMENTS.to_vec())
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
enum SurfaceSender {
    Direct(Sender<String>),
    Pumped(Sender<SurfaceMessage>),
}

enum SurfaceMessage {
    Row(String),
    Barrier(Sender<()>),
}

pub struct SurfaceOutput {
    sender: SurfaceSender,
    pending: String,
    pending_answer: bool,
    cancel: CancelToken,
    reply_started: Arc<AtomicBool>,
}

impl SurfaceOutput {
    pub fn new(sender: Sender<String>, cancel: CancelToken) -> Self {
        Self {
            sender: SurfaceSender::Direct(sender),
            pending: String::new(),
            pending_answer: false,
            cancel,
            reply_started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn pumped(
        external: Sender<String>,
        cancel: CancelToken,
        reply_started: Arc<AtomicBool>,
    ) -> (Self, std::thread::JoinHandle<()>) {
        let (sender, receiver) = crossbeam::channel::unbounded();
        let pump_cancel = cancel.clone();
        let owner = std::thread::Builder::new()
            .name("ego-output-pump".to_string())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        SurfaceMessage::Row(row) => {
                            if external.send(row).is_err() {
                                pump_cancel.cancel(CancelCause::Host);
                                break;
                            }
                        }
                        SurfaceMessage::Barrier(reply) => {
                            if external.send(String::new()).is_err() {
                                pump_cancel.cancel(CancelCause::Host);
                                break;
                            }
                            let _ = reply.send(());
                        }
                    }
                }
            })
            .expect("output pump thread");
        (
            Self {
                sender: SurfaceSender::Pumped(sender),
                pending: String::new(),
                pending_answer: false,
                cancel,
                reply_started,
            },
            owner,
        )
    }

    pub fn write(&mut self, text: &str) -> Result<(), SendError<String>> {
        self.write_with_kind(text, false)
    }

    pub(crate) fn write_answer(&mut self, text: &str) -> Result<(), SendError<String>> {
        self.write_with_kind(text, true)
    }

    fn write_with_kind(&mut self, text: &str, answer: bool) -> Result<(), SendError<String>> {
        if text.is_empty() {
            return Ok(());
        }
        self.pending.push_str(text);
        self.pending_answer |= answer;
        while let Some(end) = self.pending.find('\n').map(|index| index + 1) {
            let row: String = self.pending.drain(..end).collect();
            self.send(row)?;
            if self.pending_answer {
                self.mark_reply_started();
                self.pending_answer = false;
            }
        }
        Ok(())
    }

    pub(crate) fn mark_reply_started(&self) {
        self.reply_started.store(true, Ordering::Release);
    }

    pub fn finish(&mut self) -> Result<(), SendError<String>> {
        if !self.pending.is_empty() {
            let row = std::mem::take(&mut self.pending);
            self.send(row)?;
            if self.pending_answer {
                self.mark_reply_started();
                self.pending_answer = false;
            }
        }

        match &self.sender {
            SurfaceSender::Direct(_) => self.send(String::new()),
            SurfaceSender::Pumped(sender) => {
                let (reply, answer) = bounded(0);
                let result = sender
                    .send(SurfaceMessage::Barrier(reply))
                    .map_err(|_| SendError(String::new()))
                    .and_then(|()| answer.recv().map_err(|_| SendError(String::new())));
                result.inspect_err(|_| self.cancel.cancel(CancelCause::Host))
            }
        }
    }

    fn send(&self, row: String) -> Result<(), SendError<String>> {
        let result = match &self.sender {
            SurfaceSender::Direct(sender) => sender.send(row),
            SurfaceSender::Pumped(sender) => {
                let failed = row.clone();
                sender
                    .send(SurfaceMessage::Row(row))
                    .map_err(|_| SendError(failed))
            }
        };
        result.inspect_err(|_| self.cancel.cancel(CancelCause::Host))
    }
}

enum Control {
    InsertPasteText {
        text: String,
    },
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
        context: String,
        reply: Sender<Trust>,
    },
    WindowsUnconfined {
        workspace: String,
        reply: Sender<bool>,
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
    StartupModel {
        picker: Arc<Mutex<ModelPicker>>,
        notice: Option<String>,
        reply: Sender<PickerOutcome>,
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
    activity_worker: Mutex<Option<ActivityWorker>>,
    output_owner: Mutex<Option<std::thread::JoinHandle<()>>>,
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
    input: Mutex<std::io::BufReader<std::io::Stdin>>,
}

enum SeatKind {
    Live(Box<LiveSeat>),
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
        let registry = Arc::new(ActionRegistry::final_surface());

        if !terminal {
            return Ok((
                Self {
                    kind: SeatKind::Sequential(Box::new(SequentialSeat {
                        input: Mutex::new(std::io::BufReader::new(std::io::stdin())),
                    })),
                    terminal,
                    registry,
                    coordinator: Mutex::new(Some(control)),
                },
                None,
            ));
        }

        let phase = SurfacePhase::new(EditPhase::Busy);
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
            .with_quick_completions(true)
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(ActionMenu::new(
                ColumnarMenu::default().with_name(ACTION_MENU),
            ))))
            .use_bracketed_paste(true);

        let painter = Arc::new(InteractivePainter::live(editor.repaint_signal()));
        {
            let mut status = status.lock().expect("surface status lock");
            status.painter = painter;
            status.cursor_selectors = true;
        }
        let printer = ExternalPrinter::new(CHANNEL_CAPACITY);
        status.lock().expect("surface status lock").transcript = Some(printer.sender());
        let reply_started = Arc::clone(&status.lock().expect("surface status lock").reply_started);
        let (output, output_owner) =
            SurfaceOutput::pumped(printer.sender(), cancel.clone(), reply_started);
        let startup_output = printer.receiver().clone();
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
                        startup_output,
                    },
                );
            })?;

        let (activity_stop, activity_done) = bounded(1);
        let activity_status = Arc::downgrade(&status);
        let activity_phase = phase.clone();
        let activity_owner = std::thread::Builder::new()
            .name("ego-activity-tick".to_string())
            .spawn(move || {
                while matches!(
                    activity_done.recv_timeout(Duration::from_secs(1)),
                    Err(crossbeam::channel::RecvTimeoutError::Timeout)
                ) {
                    let Some(status) = activity_status.upgrade() else {
                        break;
                    };
                    let busy = activity_phase.get() == EditPhase::Busy
                        && status.lock().expect("surface status lock").activity;
                    if busy {
                        repaint_status(&status);
                    }
                }
            })?;

        Ok((
            Self {
                kind: SeatKind::Live(Box::new(LiveSeat {
                    input,
                    control: seat_control,
                    activated: AtomicBool::new(false),
                    break_signal,
                    phase,
                    status,
                    git_worker: Mutex::new(None),
                    activity_worker: Mutex::new(Some(ActivityWorker {
                        stop: activity_stop,
                        owner: activity_owner,
                    })),
                    output_owner: Mutex::new(Some(output_owner)),
                    owner: Mutex::new(Some(owner)),
                })),
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
                    let mut status = live.status.lock().expect("surface status lock");
                    status.begin_activity();
                    drop(status);
                    repaint_status(&live.status);
                }
                line
            }
            SeatKind::Sequential(seat) => {
                let line =
                    read_sequential_line(&mut *seat.input.lock().expect("sequential input lock"));
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
            if let Some(worker) = live
                .activity_worker
                .lock()
                .expect("activity worker lock")
                .as_ref()
            {
                let _ = worker.stop.try_send(());
            }
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
        if let Some(owner) = live.output_owner.lock().expect("output owner lock").take() {
            let _ = owner.join();
        }
        if let Some(worker) = live.git_worker.lock().expect("Git worker lock").take() {
            drop(worker.sender);
            let _ = worker.owner.join();
        }
        if let Some(worker) = live
            .activity_worker
            .lock()
            .expect("activity worker lock")
            .take()
        {
            drop(worker.stop);
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
            ActionId::Expand,
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

    /// Let the terminal owner tell the host when it is waiting on a person.
    pub fn announce_waits(&self, osc: Osc, quiet: bool) {
        if let SeatKind::Live(live) = &self.kind {
            live.status.lock().expect("surface status lock").signals = Some((osc, quiet));
        }
    }

    /// Enable policy controls after their application owner and catalogue exist.
    pub(crate) fn enable_policy_controls(&self, rules: Arc<dyn PermissionRuleApplication>) {
        for action in [ActionId::Mode, ActionId::Plan, ActionId::Yolo] {
            self.registry
                .set_availability(action, Availability::Enabled);
        }
        if let SeatKind::Live(live) = &self.kind {
            live.status
                .lock()
                .expect("surface status lock")
                .permission_rules = Some(rules);
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

    pub(crate) fn set_image_chips(&self, chips: Vec<String>) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        live.status.lock().expect("surface status lock").image_chips = chips;
        repaint_status(&live.status);
    }

    fn insert_paste_text(&self, text: String) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        if live.control.send(Control::InsertPasteText { text }).is_ok() {
            live.wake_owner();
        }
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

    /// Let the terminal owner collect one persistent first-run model choice.
    pub(crate) fn read_startup_model(
        &self,
        picker: Arc<Mutex<ModelPicker>>,
        notice: Option<String>,
    ) -> PickerOutcome {
        self.request(
            |reply| Control::StartupModel {
                picker,
                notice,
                reply,
            },
            PickerOutcome::Cancelled,
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

fn read_sequential_line(input: &mut impl BufRead) -> Line {
    let mut text = String::new();
    match input.read_line(&mut text) {
        Ok(0) | Err(_) => Line::Ended,
        Ok(_) => {
            if text.ends_with('\n') {
                text.pop();
                if text.ends_with('\r') {
                    text.pop();
                }
            }
            Line::of(reedline::Signal::Success(text))
        }
    }
}

impl crate::run::Reads for Seat {
    fn read(&self) -> Line {
        Seat::read(self)
    }
}

impl crate::run::InteractiveSeat for Seat {
    fn pasteboard_kind(&self) -> Result<crate::run::PasteboardKind, String> {
        super::clipboard::kind()
    }

    fn pasteboard_image(&self, max_bytes: u64) -> Result<Vec<u8>, String> {
        super::clipboard::image(max_bytes)
    }

    fn pasteboard_text(&self) -> Result<String, String> {
        super::clipboard::text()
    }

    fn insert_paste_text(&self, text: String) {
        Seat::insert_paste_text(self, text);
    }
    fn set_image_chips(&self, chips: Vec<String>) {
        Seat::set_image_chips(self, chips);
    }

    fn set_notice(&self, notice: String) {
        Seat::set_notice(self, notice);
    }

    fn has_human(&self) -> bool {
        self.terminal
    }

    fn width(&self) -> usize {
        match &self.kind {
            SeatKind::Live(live) => live.status.lock().expect("surface status lock").width,
            SeatKind::Sequential(_) => 80,
        }
    }

    fn action_help(&self) -> String {
        render_action_help(&self.registry, self.width().saturating_sub(4))
    }

    fn key_hints(&self) -> String {
        render_key_hints(&self.registry, self.width())
    }

    fn read_startup_model(
        &self,
        picker: Arc<Mutex<ModelPicker>>,
        notice: Option<String>,
    ) -> PickerOutcome {
        Seat::read_startup_model(self, picker, notice)
    }

    fn busy(&self) {
        self.registry
            .set_availability(ActionId::Compact, Availability::Busy);
        if let SeatKind::Live(live) = &self.kind {
            live.phase.set(EditPhase::Busy);
            let mut status = live.status.lock().expect("surface status lock");
            status.begin_activity();
            drop(status);
            repaint_status(&live.status);
        }
    }

    fn modal_wait(&self) -> std::time::Duration {
        match &self.kind {
            SeatKind::Live(live) => {
                live.status
                    .lock()
                    .expect("surface status lock")
                    .modal_waited
            }
            _ => std::time::Duration::ZERO,
        }
    }

    fn idle(&self) {
        if let SeatKind::Live(live) = &self.kind {
            // The terminal owner alone leaves Modal, even if idle arrives late.
            let _ = live.phase.0.compare_exchange(
                EditPhase::Busy as u8,
                EditPhase::Idle as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            let mut status = live.status.lock().expect("surface status lock");
            status.activity = false;
            status.activity_since = None;
            drop(status);
            repaint_status(&live.status);
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

    fn trust(&self, workspace: &Path, context: &str) -> Trust {
        // Piped stdin may itself be the first prompt. Consuming it as a trust
        // answer would turn user content into authority. With no terminal
        // there is no human seat, so the only truthful answer is untrusted.
        if !self.terminal {
            return Trust::Untrusted;
        }
        self.request(
            |reply| Control::Trust {
                workspace: workspace.display().to_string(),
                context: context.to_owned(),
                reply,
            },
            Trust::Untrusted,
        )
    }

    fn acknowledge_windows_unconfined(&self, workspace: &Path) -> bool {
        if !self.terminal {
            return false;
        }
        self.request(
            |reply| Control::WindowsUnconfined {
                workspace: workspace.display().to_string(),
                reply,
            },
            false,
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
        startup_output,
    } = owner;
    let mut pending_control = None;
    let mut active = false;
    let mut idle_interrupt = None;
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
            // Before activation Reedline is not reading, so this same owner
            // drains its bounded printer queue without opening an input prompt.
            let request = crossbeam::channel::select_biased! {
                recv(startup_output) -> row => {
                    let Ok(row) = row else { break };
                    let mut output = std::io::stdout().lock();
                    if output.write_all(row.as_bytes()).and_then(|()| output.flush()).is_err() {
                        cancel.cancel(CancelCause::Host);
                        break;
                    }
                    continue;
                }
                recv(control) -> request => request,
            };
            match request {
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
                let line = if matches!(line, Line::Interrupted) && phase.get() == EditPhase::Idle {
                    let now = std::time::Instant::now();
                    if idle_interrupt.is_some_and(|previous| {
                        now.duration_since(previous) <= DOUBLE_CTRL_C_WINDOW
                    }) {
                        Line::Ended
                    } else {
                        idle_interrupt = Some(now);
                        Line::Interrupted
                    }
                } else {
                    if !matches!(line, Line::Interrupted) {
                        idle_interrupt = None;
                    }
                    line
                };
                let ended = matches!(line, Line::Ended);
                if input.send(line).is_err() || (ended && failed) {
                    break;
                }
            }
        }
    }
}

mod action_execution;
mod edit_mode;
mod permission;
mod permission_modal;
mod picker_surface;
mod question;

#[cfg(test)]
use action_execution::apply_surface_effect;
use action_execution::surface_line;
pub(crate) use action_execution::{render_action_help, render_key_hints};
pub use edit_mode::SurfaceEditMode;
pub(crate) use permission::Confirmation;
#[cfg(test)]
use permission::{PermissionMenuId, STATUSLINE_PRESETS, permission_menu};
use permission::{
    StatuslineChoice, choose_permission_action, choose_policy_mode, choose_statusline_preset,
    configure_statusline, confirm_surface_action,
};
use picker_surface::run_model_picker;
use question::{
    PermissionEditOutcome, Question, Waiting, answer_control, edit_permission_parameters,
    enter_modal, leave_modal, next_selector_key, parse_trust, question_selector_prompt,
    queued_selector_line,
};
#[cfg(test)]
use question::{PermissionWord, parse_permission};

#[cfg(test)]
mod tests;
