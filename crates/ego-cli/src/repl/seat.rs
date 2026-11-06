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
    KeyCode, KeyModifiers, Keybindings, Menu, MenuBuilder, MenuEvent, Painter, Prompt,
    PromptEditMode, PromptHistorySearch, Reedline, ReedlineEvent, ReedlineMenu, ReedlineRawEvent,
    Span, Suggestion, Vi, default_emacs_keybindings, default_vi_insert_keybindings,
    default_vi_normal_keybindings,
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
    GitFacts, GitFactsSource, SeatFacts, SelectionFacts, StatusSegmentName, SurfaceSnapshot,
};
use super::surface::InteractivePainter;
use super::{
    BACKGROUND_COMMAND, BUSY_CANCEL_COMMAND, BUSY_RECALL_COMMAND, BUSY_STEER_COMMAND,
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
pub(super) const MODE_CYCLE_COMMAND: &str = "ego:mode-cycle";
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
    Fast,
    Export,
    Plan,
    Yolo,
    Permissions,
    Compact,
    Pin,
    Unpin,
    Refresh,
    Fork,
    Sessions,
    Usage,
    Context,
    New,
    Name,
    Goal,
    Help,
    Clear,
    Statusline,
    Mcp,
    Tasks,
    Side,
    Tools,
    Expand,
    Skills,
    Hooks,
    Doctor,
    Setup,
    Init,
    Config,
    Quit,
    Version,
}

#[derive(Clone)]
struct McpSurfaceActions {
    registry: Arc<McpRegistryController>,
    native_tools: Arc<[RegisteredTool]>,
    servers: Vec<(ego_core::config::McpServerName, bool)>,
}

struct SurfaceActionSink {
    pending: Arc<Mutex<Option<SurfaceActionEffect>>>,
}

impl ActionSink for SurfaceActionSink {
    fn invoke(&mut self, action: LocalAction) -> LocalActionOutcome {
        let effect = match action.id {
            ActionId::Mode => SurfaceActionEffect::Mode,
            ActionId::Fast => SurfaceActionEffect::Fast,
            ActionId::Plan => SurfaceActionEffect::Plan,
            ActionId::Yolo => SurfaceActionEffect::Yolo,
            ActionId::Permissions => SurfaceActionEffect::Permissions,
            ActionId::Compact => SurfaceActionEffect::Compact,
            ActionId::Pin => SurfaceActionEffect::Pin,
            ActionId::Unpin => SurfaceActionEffect::Unpin,
            ActionId::Refresh => SurfaceActionEffect::Refresh,
            ActionId::Export => SurfaceActionEffect::Export,
            ActionId::Fork => SurfaceActionEffect::Fork,
            ActionId::Sessions => SurfaceActionEffect::Sessions,
            ActionId::Usage => SurfaceActionEffect::Usage,
            ActionId::Context => SurfaceActionEffect::Context,
            ActionId::New => SurfaceActionEffect::New,
            ActionId::Name => SurfaceActionEffect::Name,
            ActionId::Goal => SurfaceActionEffect::Goal,
            ActionId::Help => SurfaceActionEffect::Help,
            ActionId::Clear => SurfaceActionEffect::Clear,
            ActionId::Statusline => SurfaceActionEffect::Statusline,
            ActionId::Mcp => SurfaceActionEffect::Mcp,
            ActionId::Tasks => SurfaceActionEffect::Tasks,
            ActionId::Side => SurfaceActionEffect::Side,
            ActionId::Tools => SurfaceActionEffect::Tools,
            ActionId::Expand => SurfaceActionEffect::Expand,
            ActionId::Skills => SurfaceActionEffect::Skills,
            ActionId::Hooks => SurfaceActionEffect::Hooks,
            ActionId::Setup => SurfaceActionEffect::Setup,
            ActionId::Doctor => SurfaceActionEffect::Doctor,
            ActionId::Init => SurfaceActionEffect::Init,
            ActionId::Config => SurfaceActionEffect::Config,
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
        if position == line.len()
            && (self.registry.resolve_slash(filter).is_some()
                || self.registry.resolve_alias(filter).is_some()
                || self
                    .registry
                    .mcp_menu(filter)
                    .iter()
                    .any(|(name, _)| name == filter)
                || self
                    .registry
                    .skill_menu(filter)
                    .iter()
                    .any(|(name, _)| name == filter))
        {
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
                    Availability::Busy | Availability::BusyWithReason { .. } => {
                        format!("{} — busy", registration.description_text())
                    }
                    Availability::Enabled => registration.description_text().to_string(),
                }),
                span: Span::new(0, position),
                append_whitespace: false,
                ..Suggestion::default()
            })
            .collect::<Vec<_>>();
        suggestions.extend(
            self.registry
                .aliases()
                .into_iter()
                .filter(|alias| alias.starts_with(filter))
                .map(|alias| Suggestion {
                    value: format!("/{alias}"),
                    description: Some(
                        if alias == "history" {
                            "/history QUERY — search sessions"
                        } else {
                            "/resume [ID] or /continue [ID] — resume a session"
                        }
                        .to_owned(),
                    ),
                    span: Span::new(0, position),
                    append_whitespace: false,
                    ..Suggestion::default()
                }),
        );
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
        suggestions.extend(self.registry.mcp_menu(filter).into_iter().map(
            |(name, description)| Suggestion {
                value: format!("/{name}"),
                description: Some(description),
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
    fn submit_on_accept(&self) -> bool {
        true
    }

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

fn live_queue(state: &SessionState) -> QueueStatus {
    let mut queue = state.queue_status();
    // Expedited delivery remains durable after cancellation. It is not an
    // interruption still in flight once its originating turn has settled.
    queue.interrupt_pending &= state
        .unacknowledged_prompts()
        .into_iter()
        .find(|prompt| Some(prompt.id()) == state.expedited_through())
        .is_some_and(|prompt| !state.has_turn_completed(prompt.turn()));
    queue
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
        SelectionFacts::new(selection.identity, selection.thinking, base.mode)
            .with_fast(selection.service_tier == ego_ai::service_tier::ServiceTier::Fast),
        seat,
        base.git,
    ));
}

fn repaint_status(status: &SharedStatus) {
    let (painter, line) = {
        let mut status = status.lock().expect("surface status lock");
        let painter = Arc::clone(&status.painter);
        if let Some(snapshot) = status.snapshot.as_mut() {
            snapshot.set_time(ego_core::event::time::Clock::now(
                &ego_tools::clock::SystemClock,
            ));
        }
        let rendered = status
            .native_status
            .then(|| status.snapshot.as_ref())
            .flatten()
            .map(|snapshot| {
                status
                    .template
                    .render(snapshot, &status.provider_values, status.width)
            });
        let line = rendered.map(|(line, diagnostic)| {
            if let Some(diagnostic) = diagnostic
                && status.template_diagnostic.is_none()
            {
                status.notice = Some(diagnostic.clone());
                status.template_diagnostic = Some(diagnostic);
            }
            line
        });
        (painter, line)
    };
    if let Some(line) = line {
        painter.paint_status(line);
    } else {
        painter.clear_status();
    }
}

mod background;
pub(crate) use background::{BackgroundActivity, BackgroundTaskSurface};

struct ChildActivity {
    name: String,
    activity: String,
    started: Instant,
    output_tokens: u64,
}

struct SurfaceStatus {
    details: Arc<crate::output::ExpansionHistory>,
    setup_cancel: Option<tokio::sync::watch::Sender<bool>>,
    queue: QueueStatus,
    drafts: usize,
    activity: bool,
    native_turn: Option<ego_core::event::id::TurnId>,
    activity_since: Option<Instant>,
    activity_output_chars: usize,
    children: std::collections::BTreeMap<ItemId, ChildActivity>,
    background_children: background::BackgroundRows,
    thinking_since: Option<Instant>,
    /// When the open modal began; the wait is the person's, not the turn's.
    modal_since: Option<Instant>,
    /// The part of the current turn spent waiting on modals.
    modal_waited: std::time::Duration,
    reply_started: Arc<AtomicBool>,
    retry: Option<(String, Instant, u64)>,
    shell: Option<crate::render::Line>,
    notice: Option<String>,
    image_chips: Vec<String>,
    images: super::ImageDraft,
    origin: InputOrigin,
    native_status: bool,
    width: usize,
    plan: Option<ego_core::plan::Plan>,
    template: super::status::StatusTemplate,
    template_diagnostic: Option<String>,
    provider_values: Vec<(StatusSegmentName, super::status::SegmentValue)>,
    config_store: Option<Arc<dyn UserConfigStore>>,
    snapshot: Option<SurfaceSnapshot>,
    status_base: Option<StatusBase>,
    selected_mode: Option<Mode>,
    painter: Arc<InteractivePainter>,
    actions: Option<Actions>,
    inspections: Option<(Sender<HostCommand>, Arc<futures::task::AtomicWaker>)>,
    model_picker: Option<Arc<Mutex<ModelPicker>>>,
    session_picker_open: bool,
    /// Keys the picker has been sent and not yet applied. The editor parses a
    /// whole burst of input before running its first command and drops the
    /// rest, so typed keys wait here instead of riding on that command.
    picker_keys: Vec<String>,
    model_selection: Option<Arc<GenerationSelection>>,
    question_selector: Option<Arc<Mutex<Selector>>>,
    permission_modal: bool,
    permission_capabilities: crate::render::Capabilities,
    permission_rules: Option<Arc<dyn PermissionRuleApplication>>,
    /// Rows committed straight to the transcript by the terminal owner, for
    /// receipts that have no run output to travel with. `None` without a live
    /// terminal.
    transcript: Option<Sender<SurfaceMessage>>,
    /// What the host is told around a modal wait, and whether `-q` is on.
    signals: Option<(Osc, bool)>,
    cursor_selectors: bool,
    mcp_actions: Option<McpSurfaceActions>,
    surface_effect: Arc<Mutex<Option<SurfaceActionEffect>>>,
}

impl SurfaceStatus {
    fn observe_native_turn(&mut self, event: &ego_core::public::PublicEvent) {
        use ego_core::public::PublicEvent;
        match event {
            PublicEvent::SessionStarted { .. } => self.native_turn = None,
            PublicEvent::TurnStarted { turn } => self.native_turn = Some(*turn),
            PublicEvent::TurnCompleted { turn, .. } if self.native_turn == Some(*turn) => {
                self.native_turn = None;
            }
            _ => {}
        }
    }

    fn begin_activity(&mut self) {
        if !self.activity {
            self.activity_since = Some(Instant::now());
            self.activity_output_chars = 0;
            self.thinking_since = None;
            self.modal_waited = std::time::Duration::ZERO;
            self.reply_started.store(false, Ordering::Release);
        }
        self.activity = true;
    }

    fn observe_generation_progress(&mut self, event: &ego_core::public::PublicEvent) {
        use ego_core::public::{ItemKind, PublicEvent};
        if !self.activity {
            return;
        }
        match event {
            PublicEvent::ItemStarted {
                item,
                kind: ItemKind::Tool,
                name: Some(name),
                text,
                ..
            } if name == "task" => {
                if let Some(args) = text
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    && let Some(name) = args.get("name").and_then(serde_json::Value::as_str)
                {
                    self.children.entry(*item).or_insert_with(|| ChildActivity {
                        name: name.to_owned(),
                        activity: "Working".to_owned(),
                        started: Instant::now(),
                        output_tokens: 0,
                    });
                }
                self.thinking_since = None;
            }
            PublicEvent::ChildProgress {
                item,
                activity,
                output_tokens,
                ..
            } => {
                if let Some(child) = self.children.get_mut(item) {
                    child.activity.clone_from(activity);
                    child.output_tokens = *output_tokens;
                }
            }
            PublicEvent::ItemCompleted {
                item,
                kind: ItemKind::Tool | ItemKind::Subagent,
                ..
            } => {
                self.children.remove(item);
            }
            PublicEvent::TurnCompleted { .. } => {
                self.children.clear();
                self.thinking_since = None;
            }
            PublicEvent::ItemDelta {
                kind: ItemKind::Reasoning,
                text,
                ..
            } => {
                self.thinking_since.get_or_insert_with(Instant::now);
                self.activity_output_chars = self
                    .activity_output_chars
                    .saturating_add(text.chars().count());
            }
            PublicEvent::ItemDelta {
                kind: ItemKind::AgentMessage,
                text,
                ..
            } => {
                self.thinking_since = None;
                self.activity_output_chars = self
                    .activity_output_chars
                    .saturating_add(text.chars().count());
            }
            PublicEvent::ItemStarted {
                kind: ItemKind::Tool,
                ..
            } => {
                self.thinking_since = None;
            }
            _ => {}
        }
    }
}

impl Default for SurfaceStatus {
    fn default() -> Self {
        Self {
            details: Arc::default(),
            setup_cancel: None,
            queue: QueueStatus::default(),
            drafts: 0,
            activity: false,
            native_turn: None,
            activity_since: None,
            activity_output_chars: 0,
            children: Default::default(),
            background_children: Default::default(),
            thinking_since: None,
            modal_since: None,
            modal_waited: std::time::Duration::ZERO,
            reply_started: Arc::new(AtomicBool::new(false)),
            retry: None,
            shell: None,
            notice: None,
            image_chips: Vec::new(),
            images: super::ImageDraft::default(),
            origin: InputOrigin::Typed,
            native_status: false,
            width: 0,
            plan: None,
            template: super::status::StatusTemplate::new(super::status::DEFAULT_TEMPLATE.into()),
            template_diagnostic: None,
            provider_values: Vec::new(),
            config_store: None,
            snapshot: None,
            status_base: None,
            selected_mode: None,
            painter: Arc::new(InteractivePainter::new()),
            actions: None,
            inspections: None,
            model_picker: None,
            session_picker_open: false,
            picker_keys: Vec::new(),
            model_selection: None,
            question_selector: None,
            permission_modal: false,
            permission_capabilities: crate::render::Capabilities::plain(),
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
    signals: Receiver<String>,
    inspections: Sender<HostCommand>,
    inspection_waker: Arc<futures::task::AtomicWaker>,
}

struct ComposerDraft {
    images: super::ImageDraft,
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

pub(crate) struct SetupCancellation {
    receiver: tokio::sync::watch::Receiver<bool>,
    status: Option<SharedStatus>,
}

impl SetupCancellation {
    pub(crate) async fn cancelled(&mut self) {
        if self.status.is_none() {
            std::future::pending::<()>().await;
        }
        let _ = self.receiver.changed().await;
    }
}

impl Drop for SetupCancellation {
    fn drop(&mut self) {
        if let Some(status) = &self.status {
            status.lock().expect("surface status lock").setup_cancel = None;
        }
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
    pub template: String,
    pub width: usize,
    pub native: bool,
    pub git: Option<Arc<dyn GitFactsSource>>,
}

pub(crate) const DEFAULT_STATUSLINE_SEGMENTS: &[ego_core::config::StatusSegmentName] = &[
    ego_core::config::StatusSegmentName::Model,
    ego_core::config::StatusSegmentName::Effort,
    ego_core::config::StatusSegmentName::Fast,
    ego_core::config::StatusSegmentName::Mode,
    ego_core::config::StatusSegmentName::Context,
    ego_core::config::StatusSegmentName::Cost,
    ego_core::config::StatusSegmentName::Cache,
    ego_core::config::StatusSegmentName::Queue,
    ego_core::config::StatusSegmentName::Drafts,
    ego_core::config::StatusSegmentName::Interrupt,
    ego_core::config::StatusSegmentName::Cwd,
    ego_core::config::StatusSegmentName::Git,
    ego_core::config::StatusSegmentName::Trust,
];

pub(crate) fn effective_statusline_template(
    configured: Option<&ego_core::config::StatuslineLayer>,
) -> String {
    configured
        .and_then(|statusline| statusline.template.clone())
        .unwrap_or_else(|| super::status::DEFAULT_TEMPLATE.to_owned())
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

    /// Publish driver activity without taking input away from a modal.
    fn set_activity(&self, phase: EditPhase) {
        let _ = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current != EditPhase::Modal as u8).then_some(phase as u8)
            });
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
    pub(crate) details: Arc<crate::output::ExpansionHistory>,
    sender: SurfaceSender,
    pending: String,
    pending_answer: bool,
    shell: Option<String>,
    running_tools: Vec<(ego_core::event::id::ItemId, crate::render::Line)>,
    status: Option<SharedStatus>,
    answer_block: bool,
    wrote: bool,
    blank: bool,
    signals: Option<Sender<String>>,
    cancel: CancelToken,
    reply_started: Arc<AtomicBool>,
}

impl SurfaceOutput {
    pub fn new(sender: Sender<String>, cancel: CancelToken) -> Self {
        Self {
            details: Arc::default(),
            sender: SurfaceSender::Direct(sender),
            pending: String::new(),
            pending_answer: false,
            shell: None,
            running_tools: Vec::new(),
            status: None,
            answer_block: false,
            wrote: false,
            blank: false,
            signals: None,
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
                let mut pending = None;
                while let Some(message) = pending.take().or_else(|| receiver.recv().ok()) {
                    match message {
                        SurfaceMessage::Row(mut row) => {
                            // One printer handoff causes one repaint and cursor query.
                            // Drain already queued rows together, but never cross a
                            // finish barrier or change a message's line boundary.
                            for _ in 0..receiver.len() {
                                let Ok(message) = receiver.try_recv() else {
                                    break;
                                };
                                match message {
                                    SurfaceMessage::Row(next) => {
                                        if !row.is_empty() && !row.ends_with('\n') {
                                            row.push('\n');
                                        }
                                        row.push_str(&next);
                                    }
                                    barrier @ SurfaceMessage::Barrier(_) => {
                                        pending = Some(barrier);
                                        break;
                                    }
                                }
                            }
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
                details: Arc::default(),
                sender: SurfaceSender::Pumped(sender),
                pending: String::new(),
                pending_answer: false,
                shell: None,
                running_tools: Vec::new(),
                status: None,
                answer_block: false,
                wrote: false,
                blank: false,
                signals: None,
                cancel,
                reply_started,
            },
            owner,
        )
    }

    pub(crate) fn plan_status(&self, plan: &ego_core::plan::Plan) -> bool {
        let Some(status) = &self.status else {
            return false;
        };
        status.lock().expect("surface status lock").plan = Some(plan.clone());
        repaint_status(status);
        true
    }

    pub(crate) fn take_plan(&self) -> Option<ego_core::plan::Plan> {
        let status = self.status.as_ref()?;
        let plan = status.lock().expect("surface status lock").plan.take();
        repaint_status(status);
        plan
    }

    pub(crate) fn goal_status(&self, goal: Option<(String, u32, Option<u32>)>) {
        if let Some(status) = &self.status {
            if let Some(snapshot) = status
                .lock()
                .expect("surface status lock")
                .snapshot
                .as_mut()
            {
                snapshot.set_goal(goal);
            }
            repaint_status(status);
        }
    }

    pub(crate) fn retry_status(&mut self, message: Option<(&str, u64)>) {
        if message.is_some() {
            let _ = self.settle_shell();
        }
        if let Some(status) = &self.status {
            status.lock().expect("surface status lock").retry =
                message.map(|(text, delay)| (text.to_string(), Instant::now(), delay));
            repaint_status(status);
        }
    }

    pub(crate) fn signal(&self, sequence: &str) -> Result<(), SendError<String>> {
        match &self.signals {
            Some(signals) => signals.send(sequence.to_owned()),
            None => self.send(sequence.to_owned()),
        }
    }

    pub fn write(&mut self, text: &str) -> Result<(), SendError<String>> {
        self.settle_shell()?;
        self.write_with_kind(text, false)
    }

    pub(crate) fn start_tool(
        &mut self,
        item: ego_core::event::id::ItemId,
        line: crate::render::Line,
    ) {
        self.running_tools.push((item, line));
        self.repaint_running_tool();
    }

    pub(crate) fn finish_tool(&mut self, item: ego_core::event::id::ItemId) {
        self.running_tools.retain(|(id, _)| *id != item);
        self.repaint_running_tool();
    }

    pub(crate) fn clear_tools(&mut self) {
        if !self.running_tools.is_empty() {
            self.running_tools.clear();
            self.repaint_running_tool();
        }
    }

    fn repaint_running_tool(&self) {
        if let Some(status) = &self.status {
            status.lock().expect("surface status lock").shell =
                self.running_tools.first().map(|(_, line)| line.clone());
            repaint_status(status);
        }
    }

    pub(crate) fn write_shell(
        &mut self,
        rows: &str,
        live: crate::render::Line,
    ) -> Result<(), SendError<String>> {
        let Some(status) = &self.status else {
            return self.write_tool(rows);
        };
        self.shell = Some(rows.to_owned());
        status.lock().expect("surface status lock").shell = Some(
            self.running_tools
                .first()
                .map(|(_, line)| line.clone())
                .unwrap_or(live),
        );
        repaint_status(status);
        Ok(())
    }

    pub(crate) fn settle_shell(&mut self) -> Result<(), SendError<String>> {
        if let Some(rows) = self.shell.take() {
            if let Some(status) = &self.status {
                status.lock().expect("surface status lock").shell =
                    self.running_tools.first().map(|(_, line)| line.clone());
                repaint_status(status);
            }
            self.write_tool(&rows)?;
        }
        Ok(())
    }

    pub(crate) fn begin_block(&mut self) -> Result<(), SendError<String>> {
        self.settle_shell()?;
        if !self.pending.is_empty() {
            self.write_with_kind("\n", false)?;
        }
        if self.wrote && !self.blank {
            self.write_with_kind("\n", false)?;
        }
        self.answer_block = false;
        Ok(())
    }

    pub(crate) fn write_tool(&mut self, text: &str) -> Result<(), SendError<String>> {
        self.begin_block()?;
        if !self.wrote {
            self.write_with_kind("\n", false)?;
        }
        self.write_with_kind(text, false)?;
        self.write_with_kind("\n", false)
    }

    pub(crate) fn write_answer(&mut self, text: &str) -> Result<(), SendError<String>> {
        if text.is_empty() {
            return Ok(());
        }
        self.settle_shell()?;
        let text = if !self.answer_block {
            self.begin_block()?;
            if !self.wrote {
                self.write_with_kind("\n", false)?;
            }
            self.answer_block = true;
            text.trim_start_matches('\n')
        } else {
            text
        };
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
            self.blank = row.trim().is_empty();
            self.wrote = true;
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
        self.settle_shell()?;
        self.answer_block = false;
        if !self.pending.is_empty() {
            let row = std::mem::take(&mut self.pending);
            self.blank = row.trim().is_empty();
            self.wrote = true;
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
    SessionPicker {
        sessions: Vec<ego_core::store::session_store::SessionMeta>,
        reply: Sender<Option<SessionId>>,
    },
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
    SelectSetup {
        request: QuestionRequest,
        reply: oneshot::Sender<QuestionOutcome>,
    },
    ConfirmSetup {
        text: String,
        reply: Sender<Confirmation>,
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
    inspections: Receiver<HostCommand>,
    background_ready: Receiver<()>,
    background_wake: Sender<()>,
    inspection_waker: Arc<futures::task::AtomicWaker>,
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

/// Keep the surface controls identical in Emacs, Vim insert and Vim normal.
fn composer_keybindings(mut keybindings: Keybindings) -> Keybindings {
    for (modifiers, code) in [
        (KeyModifiers::CONTROL, KeyCode::Char('j')),
        (KeyModifiers::CONTROL, KeyCode::Enter),
        (KeyModifiers::SHIFT, KeyCode::Enter),
        (KeyModifiers::ALT, KeyCode::Enter),
    ] {
        keybindings.add_binding(
            modifiers,
            code,
            ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
        );
    }
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(ACTION_MENU.to_owned()),
            ReedlineEvent::MenuNext,
        ]),
    );
    keybindings
}

fn emacs_composer() -> Emacs {
    let mut keybindings = composer_keybindings(default_emacs_keybindings());
    add_action_menu(&mut keybindings);
    Emacs::new(keybindings)
}

fn add_action_menu(keybindings: &mut Keybindings) {
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Char('/'),
        ReedlineEvent::Multiple(vec![
            ReedlineEvent::Edit(vec![EditCommand::InsertChar('/')]),
            ReedlineEvent::Menu(ACTION_MENU.to_owned()),
        ]),
    );
}

fn composer_editor(vim: bool) -> Box<dyn EditMode> {
    if vim {
        let mut insert = composer_keybindings(default_vi_insert_keybindings());
        add_action_menu(&mut insert);
        Box::new(Vi::new(
            insert,
            composer_keybindings(default_vi_normal_keybindings()),
        ))
    } else {
        Box::new(emacs_composer())
    }
}

impl Seat {
    pub(crate) fn observe_native_turn(&self, event: &ego_core::public::PublicEvent) {
        if let SeatKind::Live(live) = &self.kind {
            let mut status = live.status.lock().expect("surface status lock");
            status.observe_native_turn(event);
        }
    }

    pub(crate) fn observe_generation_progress(&self, event: &ego_core::public::PublicEvent) {
        if let SeatKind::Live(live) = &self.kind {
            live.status
                .lock()
                .expect("surface status lock")
                .observe_generation_progress(event);
        }
    }

    /// Build the applicable editor and, for a terminal, its only output port.
    ///
    /// Bracketed paste is not a nicety: without it a pasted stack trace submits its first
    /// line as a whole prompt, which is the failure that makes pasting one impossible.
    pub fn new(cancel: CancelToken) -> std::io::Result<(Self, Option<SurfaceOutput>)> {
        Self::with_keyboard(cancel, &ego_core::config::UiLayer::default())
    }

    pub(crate) fn with_keyboard(
        cancel: CancelToken,
        preferences: &ego_core::config::UiLayer,
    ) -> std::io::Result<(Self, Option<SurfaceOutput>)> {
        let registry = Arc::new(
            ActionRegistry::final_surface()
                .with_keybindings(
                    preferences
                        .keybindings
                        .as_ref()
                        .unwrap_or(&Default::default()),
                )
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?,
        );
        let (ingress, control) = interactive_control();
        let terminal = std::io::stdin().is_terminal();
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
        let mode = SurfaceEditMode::with_editor(
            composer_editor(preferences.vim.unwrap_or(false)),
            phase.clone(),
            Arc::clone(&status),
        );
        let asked_for_image = mode.asked_for_image();
        let mut editor = Reedline::create()
            .with_edit_mode(Box::new(mode))
            .with_highlighter(Box::new(background_notice::NoticeHighlighter(Arc::clone(
                &status,
            ))))
            .with_completer(Box::new(ActionCompleter {
                registry: Arc::clone(&registry),
            }))
            .with_quick_completions(true)
            // Erasing inside a /-prefixed draft re-filters the menu; ActionMenu closes it
            // once the completer has no choice left (for example after the / is erased).
            .with_persistent_menus(true)
            .with_menu(ReedlineMenu::EngineCompleter(Box::new(ActionMenu::new(
                ColumnarMenu::default()
                    .with_name(ACTION_MENU)
                    .with_text_style(reedline::Color::Default.normal())
                    .with_description_text_style(reedline::Color::Default.normal())
                    .with_selected_text_style(reedline::Color::Default.reverse())
                    .with_selected_match_text_style(reedline::Color::Default.reverse().underline()),
            ))))
            .use_bracketed_paste(true)
            .use_kitty_keyboard_enhancement(true);

        // Keep startup type-ahead as Enter bytes, without cooked tty echo.
        // Reedline restores the terminal on drop, including startup refusal.
        crossterm::terminal::enable_raw_mode()?;

        let painter = Arc::new(InteractivePainter::live(editor.repaint_signal()));
        {
            let mut status = status.lock().expect("surface status lock");
            status.painter = painter;
            status.cursor_selectors = true;
        }
        let printer = ExternalPrinter::new(CHANNEL_CAPACITY);
        let reply_started = Arc::clone(&status.lock().expect("surface status lock").reply_started);
        let (mut output, output_owner) =
            SurfaceOutput::pumped(printer.sender(), cancel.clone(), reply_started);
        // Host metadata never waits for the row printer or consumes a row.
        let (signal_sender, signals) = crossbeam::channel::unbounded();
        output.signals = Some(signal_sender);
        output.details = Arc::clone(&status.lock().expect("surface status lock").details);
        output.status = Some(Arc::clone(&status));
        let active_signals = signals.clone();
        editor = editor.with_geometry_idle_callback(Box::new(move || {
            let mut out = std::io::stdout().lock();
            let mut wrote = false;
            for sequence in active_signals.try_iter() {
                wrote = true;
                if out.write_all(sequence.as_bytes()).is_err() {
                    return true;
                }
            }
            // These OSC metadata signals do not move the editing cursor.
            // A failed write may leave an incomplete control sequence.
            wrote && out.flush().is_err()
        }));
        if let SurfaceSender::Pumped(sender) = &output.sender {
            status.lock().expect("surface status lock").transcript = Some(sender.clone());
        }
        #[cfg(test)]
        let held_printer = printer.sender();
        let startup_output = printer.receiver().clone();
        let break_signal = Arc::new(AtomicBool::new(false));
        // The driver can wait for the output pump at turn completion. Its
        // terminal owner must remain free to drain that pump's printer even
        // when several input lines arrive before the driver reads again.
        let (input_sender, input) = crossbeam::channel::unbounded();
        let (inspection_sender, inspections) = crossbeam::channel::unbounded();
        let inspection_waker = Arc::new(futures::task::AtomicWaker::new());
        status.lock().expect("surface status lock").inspections =
            Some((inspection_sender.clone(), Arc::clone(&inspection_waker)));
        let owner_inspection_waker = Arc::clone(&inspection_waker);
        let (seat_control, control_receiver) = bounded(CHANNEL_CAPACITY);
        let owner_break = Arc::clone(&break_signal);
        let owner_phase = phase.clone();
        let owner_status = Arc::clone(&status);
        let owner = std::thread::Builder::new()
            .name("ego-reedline".to_string())
            .spawn(move || {
                #[cfg(test)]
                HELD_PRINTER.with(|slot| *slot.borrow_mut() = Some(held_printer));
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
                        signals,
                        inspections: inspection_sender,
                        inspection_waker: owner_inspection_waker,
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
                    let repaint = {
                        let state = status.lock().expect("surface status lock");
                        (activity_phase.get() == EditPhase::Busy && state.activity)
                            || !state.background_children.rows.is_empty()
                            || state
                                .snapshot
                                .as_ref()
                                .is_some_and(|snapshot| snapshot.cache_is_live())
                    };
                    if repaint {
                        repaint_status(&status);
                    }
                }
            })?;

        let (background_wake, background_ready) = bounded(1);
        Ok((
            Self {
                kind: SeatKind::Live(Box::new(LiveSeat {
                    input,
                    inspections,
                    background_ready,
                    background_wake,
                    inspection_waker,
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

    /// Read one live line, or `None` when `timeout` fires first.
    fn read_live(live: &LiveSeat, timeout: &Receiver<Instant>) -> Option<Line> {
        // Already admitted inspections belong to the current session, so
        // answer them before ordinary input can switch or close it.
        let line = crossbeam::channel::select_biased! {
            recv(live.inspections) -> command => command.map(Line::HostCommand).unwrap_or(Line::Ended),
            recv(live.input) -> line => line.unwrap_or(Line::Ended),
            recv(live.background_ready) -> _ => Line::HostCommand(HostCommand::BackgroundExecSettled),
            recv(timeout) -> _ => return None,
        };
        if matches!(
            line,
            Line::Submitted(_) | Line::HostCommand(HostCommand::SubmittedDraft { .. })
        ) {
            let mut status = live.status.lock().expect("surface status lock");
            status.begin_activity();
            live.phase.set_activity(EditPhase::Busy);
            drop(status);
            repaint_status(&live.status);
        }
        Some(line)
    }

    /// Read one line, or `None` when a live seat stays idle until `deadline`.
    /// A sequential seat has no idle period, so it never times out.
    pub fn read_before(&self, deadline: Instant) -> Option<Line> {
        match &self.kind {
            SeatKind::Live(live) => Self::read_live(live, &crossbeam::channel::at(deadline)),
            SeatKind::Sequential(_) => Some(self.read()),
        }
    }

    /// Read one line.
    pub fn read(&self) -> Line {
        match &self.kind {
            SeatKind::Live(live) => Self::read_live(live, &crossbeam::channel::never())
                .expect("a read without a deadline cannot time out"),
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
                    Dispatch::Details(action) => Line::HostCommand(HostCommand::Details(action)),
                    Dispatch::History(query) => Line::HostCommand(HostCommand::History(query)),
                    Dispatch::Resume(id) => Line::HostCommand(HostCommand::Resume(id)),
                    Dispatch::SessionMetadata(operation) => {
                        Line::HostCommand(HostCommand::SessionMetadata(operation))
                    }
                    Dispatch::Skill(operation) => Line::HostCommand(HostCommand::Skill(operation)),
                    Dispatch::Skills(action) => Line::HostCommand(HostCommand::Skills(action)),
                    Dispatch::Export(selection) => {
                        Line::HostCommand(HostCommand::Export(selection))
                    }
                    Dispatch::Fork(item) => Line::HostCommand(HostCommand::Fork(item)),
                    Dispatch::Hooks => Line::HostCommand(HostCommand::Hooks),
                    Dispatch::Tasks(arguments) => Line::HostCommand(HostCommand::Tasks(arguments)),
                    Dispatch::Side(question) => {
                        Line::HostCommand(HostCommand::SideQuestion(question))
                    }
                    Dispatch::Pins(command) => Line::HostCommand(HostCommand::Pins(command)),
                    Dispatch::McpContext(command) => {
                        Line::HostCommand(HostCommand::McpContext(command))
                    }
                    Dispatch::Mcp(arguments) => Line::HostCommand(HostCommand::Mcp(arguments)),
                    Dispatch::Setup(arguments) => Line::HostCommand(HostCommand::Setup(arguments)),
                    Dispatch::Init(arguments) => Line::HostCommand(HostCommand::Init(arguments)),
                    Dispatch::Fast(enabled) => Line::HostCommand(HostCommand::Fast(enabled)),
                    Dispatch::Mode(mode) => {
                        Line::HostCommand(HostCommand::Policy(PolicyControlRequest::SetMode(mode)))
                    }
                    Dispatch::Config(arguments) => {
                        Line::HostCommand(HostCommand::Config(arguments))
                    }
                    Dispatch::UnknownSlash(message) => {
                        eprintln!("{message}");
                        Line::Blank
                    }
                    Dispatch::Refused { reason } => Line::Refused(reason),
                    Dispatch::Action(ActionId::Config) => {
                        Line::HostCommand(HostCommand::Action(ActionId::Config))
                    }
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
        // The terminal owner has stopped producing receipts; release its pump sender.
        live.status.lock().expect("surface status lock").transcript = None;
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
            ActionId::Fast,
            ActionId::Plan,
            ActionId::Yolo,
            ActionId::Permissions,
            ActionId::Compact,
            ActionId::Pin,
            ActionId::Unpin,
            ActionId::Refresh,
            ActionId::Export,
            ActionId::Fork,
            ActionId::Sessions,
            ActionId::Usage,
            ActionId::Context,
            ActionId::New,
            ActionId::Name,
            ActionId::Goal,
            ActionId::Help,
            ActionId::Clear,
            ActionId::Statusline,
            ActionId::Mcp,
            ActionId::Tasks,
            ActionId::Side,
            ActionId::Tools,
            ActionId::Expand,
            ActionId::Skills,
            ActionId::Hooks,
            ActionId::Init,
            ActionId::Retry,
            ActionId::Config,
            ActionId::Doctor,
            ActionId::Setup,
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
                servers: mcp_registry
                    .status()
                    .servers()
                    .iter()
                    .map(|server| (server.name().clone(), false))
                    .collect(),
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

    /// Keep the busy inventory bound to the same session-local enablement as idle commands.
    pub(crate) fn set_mcp_session_servers(
        &self,
        servers: Vec<(ego_core::config::McpServerName, bool)>,
    ) {
        if let SeatKind::Live(live) = &self.kind {
            let mut status = live.status.lock().expect("surface status lock");
            if let Some(actions) = status.mcp_actions.as_mut() {
                actions.servers = servers;
            }
        }
    }

    /// Let the terminal owner tell the host when it is waiting on a person.
    pub fn announce_waits(&self, capabilities: crate::render::Capabilities, quiet: bool) {
        if let SeatKind::Live(live) = &self.kind {
            let mut status = live.status.lock().expect("surface status lock");
            status.signals = Some((Osc::new(capabilities), quiet));
            status.permission_capabilities = capabilities;
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

    /// The last accepted selection, including one staged for the next boundary.
    pub(crate) fn set_selected_mode(&self, mode: Mode) {
        if let SeatKind::Live(live) = &self.kind {
            live.status
                .lock()
                .expect("surface status lock")
                .selected_mode = Some(mode);
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
        commit_to_transcript(&live.status, &notice);
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

    /// Publish cached results from the epoch-owned guarded status producers.
    pub(crate) fn set_status_providers(&self, values: Vec<(String, String)>) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        live.status
            .lock()
            .expect("surface status lock")
            .provider_values = values
            .into_iter()
            .map(|(name, value)| {
                (
                    StatusSegmentName::custom(name),
                    super::status::SegmentValue::text(value),
                )
            })
            .collect();
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
            template,
            width,
            native,
            git,
        } = setup;
        {
            let mut status = live.status.lock().expect("surface status lock");
            if status
                .status_base
                .as_ref()
                .is_none_or(|base| base.session != session)
            {
                status.selected_mode = None;
            }
            status.native_status = native;
            status.width = width;
            status.template = super::status::StatusTemplate::new(template);
            status.template_diagnostic = None;
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
            status.native_status
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

    /// Project the session's cache authority; the seat never resets a deadline.
    pub(crate) fn observe_prompt_cache(&self, state: &SessionState) {
        let SeatKind::Live(live) = &self.kind else {
            return;
        };
        let changed = live
            .status
            .lock()
            .expect("surface status lock")
            .snapshot
            .as_mut()
            .is_some_and(|snapshot| snapshot.set_prompt_cache(state.prompt_cache.clone()));
        if changed {
            repaint_status(&live.status);
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

    /// Own a transient cancellation route instead of the ordinary turn ingress.
    pub(crate) fn setup_cancellation(&self) -> SetupCancellation {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let status = match &self.kind {
            SeatKind::Live(live) => {
                live.status
                    .lock()
                    .expect("surface status lock")
                    .setup_cancel = Some(sender);
                Some(Arc::clone(&live.status))
            }
            _ => None,
        };
        SetupCancellation { receiver, status }
    }

    pub(crate) fn authorization_browser_command(url: &str) -> Option<std::process::Command> {
        question::authorization_browser_command(url)
    }

    /// Transient host selection: no model tool question or durable event.
    pub(crate) fn select_setup(&self, request: QuestionRequest) -> QuestionOutcome {
        futures::executor::block_on(self.select_host(request))
    }

    /// Transient host selection that can be dropped on cancellation or deadline.
    pub(crate) async fn select_host(&self, request: QuestionRequest) -> QuestionOutcome {
        let SeatKind::Live(live) = &self.kind else {
            return QuestionOutcome::Unavailable;
        };
        struct PendingModal<'a> {
            live: &'a LiveSeat,
            active: bool,
        }
        impl Drop for PendingModal<'_> {
            fn drop(&mut self) {
                if self.active {
                    self.live.wake_owner();
                }
            }
        }
        let (reply, answer) = oneshot::channel();
        if live
            .control
            .send(Control::SelectSetup { request, reply })
            .is_err()
        {
            return QuestionOutcome::Unavailable;
        }
        let mut pending = PendingModal { live, active: true };
        live.wake_owner();
        let outcome = answer.await.unwrap_or(QuestionOutcome::Unavailable);
        pending.active = false;
        outcome
    }

    pub(crate) fn confirm_setup(&self, text: String) -> Confirmation {
        if !self.terminal {
            return Confirmation::Declined;
        }
        self.request(
            |reply| Control::ConfirmSetup { text, reply },
            Confirmation::Declined,
        )
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
    read_sequential_line_reporting(input, &mut crate::output::note)
}

/// [`read_sequential_line`], naming through `report` the cause of a read that
/// failed.
fn read_sequential_line_reporting(input: &mut impl BufRead, report: &mut dyn FnMut(&str)) -> Line {
    let mut text = String::new();
    match input.read_line(&mut text) {
        Ok(0) => Line::Ended,
        Err(error) => {
            super::report_input_failure(&error, report);
            Line::Ended
        }
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
    fn read_before(&self, deadline: Instant) -> Option<Line> {
        Seat::read_before(self, deadline)
    }

    fn wake_background_exec(&self) {
        if let SeatKind::Live(live) = &self.kind {
            // One token covers every ready snapshot; no process task waits
            // for the input owner, including while a generation is active.
            let _ = live.background_wake.try_send(());
        }
    }

    fn pasteboard_kind(&self) -> Result<crate::run::PasteboardKind, String> {
        #[cfg(test)]
        if std::env::var_os("EGO_IMAGE_TEST_PTY_CHILD").is_some() {
            return Ok(crate::run::PasteboardKind::Image);
        }
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
    fn image_draft(&self) -> Option<super::ImageDraft> {
        match &self.kind {
            SeatKind::Live(live) => Some(
                live.status
                    .lock()
                    .expect("surface status lock")
                    .images
                    .clone(),
            ),
            SeatKind::Sequential(_) => None,
        }
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
        let width = self.width();
        let width = if width == 0 {
            crossterm::terminal::size()
                .map(|(cols, _)| usize::from(cols))
                .unwrap_or(0)
        } else {
            width
        };
        render_key_hints(&self.registry, width)
    }

    fn read_startup_model(
        &self,
        picker: Arc<Mutex<ModelPicker>>,
        notice: Option<String>,
    ) -> PickerOutcome {
        Seat::read_startup_model(self, picker, notice)
    }

    fn busy(&self) {
        self.registry.set_availability(
            ActionId::Compact,
            Availability::BusyWithReason {
                reason: "ego is busy; a turn is running",
            },
        );
        if let SeatKind::Live(live) = &self.kind {
            let mut status = live.status.lock().expect("surface status lock");
            status.begin_activity();
            live.phase.set_activity(EditPhase::Busy);
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
            let mut status = live.status.lock().expect("surface status lock");
            status.activity = false;
            live.phase.set_activity(EditPhase::Idle);
            status.activity_since = None;
            status.children.clear();
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

    fn restore_control(&self, control: InteractiveControl) {
        let mut owner = self
            .coordinator
            .lock()
            .expect("interactive coordinator lock");
        assert!(
            owner.is_none(),
            "only the completed driver restores its coordinator"
        );
        *owner = Some(control);
    }

    fn next_inspection(&self) -> BoxFuture<'_, HostCommand> {
        let SeatKind::Live(live) = &self.kind else {
            return Box::pin(futures::future::pending());
        };
        Box::pin(futures::future::poll_fn(move |cx| {
            live.inspection_waker.register(cx.waker());
            match live.inspections.try_recv() {
                Ok(command) => std::task::Poll::Ready(command),
                Err(_) => std::task::Poll::Pending,
            }
        }))
    }

    fn refresh_queue(&self, state: &SessionState) {
        if let SeatKind::Live(live) = &self.kind {
            let queue = live_queue(state);
            let mut status = live.status.lock().expect("surface status lock");
            if status.queue == queue {
                return;
            }
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

    fn pick_session(
        &self,
        sessions: Vec<ego_core::store::session_store::SessionMeta>,
    ) -> Option<SessionId> {
        self.request(|reply| Control::SessionPicker { sessions, reply }, None)
    }

    fn activate_session(&self, target: SessionId) -> bool {
        if let SeatKind::Live(live) = &self.kind {
            let mut status = live.status.lock().expect("surface status lock");
            status.selected_mode = None;
        }
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
        signals,
        inspections,
        inspection_waker,
    } = owner;
    let mut pending_control = None;
    let mut active = false;
    let mut idle_interrupt = None;
    let mut drafts = DraftStash::new(SessionId::new());
    loop {
        // Reedline's default indicator colour otherwise leaks into unstyled
        // retry rows even when the renderer's capability gate forbids colour.
        editor = editor.with_ansi_colors(
            status
                .lock()
                .expect("surface status lock")
                .permission_capabilities
                .coloured(),
        );
        if matches!(pending_control.as_ref(), Some(Control::Activate { .. })) {
            let Some(Control::Activate { session }) = pending_control.take() else {
                unreachable!("the pending activation was just matched");
            };
            drafts.activate(session);
            active = true;
            continue;
        }
        if !active && pending_control.is_none() {
            // A startup modal's read_line restores cooked mode on return.
            if let Err(error) = crossterm::terminal::enable_raw_mode() {
                report_terminal_failure("raw mode", error);
                cancel.cancel(CancelCause::Host);
                break;
            }
            // Before activation Reedline is not reading, so this same owner
            // drains its bounded printer queue without opening an input prompt.
            let request = crossbeam::channel::select_biased! {
                recv(signals) -> sequence => {
                    let Ok(sequence) = sequence else { break };
                    let mut out = std::io::stdout().lock();
                    if let Err(error) = out.write_all(sequence.as_bytes()).and_then(|()| out.flush()) {
                        report_terminal_failure("output", error);
                        cancel.cancel(CancelCause::Host);
                        break;
                    }
                    continue;
                }
                recv(startup_output) -> row => {
                    let Ok(row) = row else { break };
                    if let Err(error) = write_printer_message(&mut std::io::stdout().lock(), &row) {
                        report_terminal_failure("output", error);
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
        let image_operation = match &line {
            Line::HostCommand(HostCommand::ClipboardPaste) => {
                Some(super::ImageOperation::Clipboard)
            }
            Line::HostCommand(HostCommand::PastePath(path)) => {
                Some(super::ImageOperation::Path(path.clone()))
            }
            Line::HostCommand(HostCommand::AdmitPrompt { .. }) => {
                Some(super::ImageOperation::Admit)
            }
            _ => None,
        };
        let line = if let Some(operation) = image_operation {
            let draft = status.lock().expect("surface status lock").images.clone();
            let (request, answer) = super::ImageRequest::new(draft.clone(), operation);
            // Retire the suspended prompt before draining output. Resuming it
            // without retiring its footprint can leave a second visible draft.
            // A late cursor report must not abandon admission. Erasure is
            // best-effort; accept_buffer still repaints the accepted prompt.
            let _ = editor.erase_last_prompt();
            if inspections.send(HostCommand::Images(request)).is_err() {
                break;
            }
            inspection_waker.wake();
            let mut surface = ModalSurface {
                phase: &phase,
                status: &status,
                asked_for_image: &asked_for_image,
                ingress: &ingress,
                drafts: &mut drafts,
            };
            let Some(result) = wait_image_reply(
                &mut editor,
                &phase,
                &mut surface,
                &control,
                &mut pending_control,
                &startup_output,
                &signals,
                &answer,
            ) else {
                break;
            };
            let images = match result {
                Ok(images) => images,
                Err(error) => {
                    commit_to_transcript(&status, &error);
                    repaint_status(&status);
                    continue;
                }
            };
            {
                let mut current = status.lock().expect("surface status lock");
                current.image_chips = current.images.labels();
            }
            let Line::HostCommand(HostCommand::AdmitPrompt { text, steer }) = line else {
                repaint_status(&status);
                continue;
            };
            let bang = images
                .is_empty()
                .then(|| Line::bang_from_editor(&text))
                .flatten();
            if phase.get() == EditPhase::Busy && action_execution::refuse_busy_shell(&status, &text)
            {
                continue;
            }
            let accepted = if phase.get() == EditPhase::Busy {
                match futures::executor::block_on(ingress.submit(text.clone(), images.clone())) {
                    Ok(queue) => {
                        let mut current = status.lock().expect("surface status lock");
                        current.queue = queue;
                        if let Some(snapshot) = current.snapshot.as_mut() {
                            snapshot.set_queue(queue);
                        }
                        true
                    }
                    Err(ego_core::turn::ControlError::Closed) => false,
                    Err(error) => {
                        commit_to_transcript(&status, &error.to_string());
                        repaint_status(&status);
                        continue;
                    }
                }
            } else {
                false
            };
            if let Err(error) = editor.accept_buffer(&Waiting::new(Arc::clone(&status))) {
                report_terminal_failure("output", error);
                break;
            }
            if let Some(label) = crate::render::attachments::label(images.len()) {
                commit_to_transcript(&status, &label);
            }
            draft.clear();
            {
                let mut current = status.lock().expect("surface status lock");
                current.images = super::ImageDraft::default();
                current.image_chips.clear();
                current.origin = InputOrigin::Typed;
                current.notice = None;
            }
            restore_latest_draft(&mut editor, &status, &mut drafts);
            if accepted {
                if steer {
                    request_interrupt(&status, &ingress);
                }
                repaint_status(&status);
                continue;
            }
            bang.unwrap_or(Line::HostCommand(HostCommand::SubmittedDraft {
                text,
                images,
            }))
        } else if matches!(line, Line::HostCommand(HostCommand::RemovePasteImage)) {
            let mut current = status.lock().expect("surface status lock");
            current.images.pop();
            current.image_chips = current.images.labels();
            drop(current);
            repaint_status(&status);
            continue;
        } else {
            line
        };
        if phase.get() == EditPhase::Busy
            && let Line::HostCommand(command) = &line
            && (command.is_read_only_inspection() || command.is_root_revocation())
        {
            if inspections.send(command.clone()).is_err() {
                break;
            }
            inspection_waker.wake();
            continue;
        }
        match line {
            Line::ExternalBreak(_) => match control.try_recv() {
                Ok(Control::Shutdown) | Err(crossbeam::channel::TryRecvError::Disconnected) => {
                    break;
                }
                // The image rendezvous may already have serviced this wake's
                // control message. A wake is not another queued request.
                Err(crossbeam::channel::TryRecvError::Empty) => continue,
                Ok(Control::Activate { session }) => {
                    drafts.activate(session);
                    continue;
                }
                Ok(request) => {
                    // The composer stepped aside for this request: whatever
                    // asks next paints on its rows instead of below them, and
                    // the composer returns there afterwards with its draft.
                    let _ = editor.erase_last_prompt();
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
                if matches!(
                    line,
                    Line::Submitted(_)
                        | Line::HostCommand(HostCommand::SubmittedDraft { .. })
                        | Line::Ended
                ) {
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
                if phase.get() == EditPhase::Busy
                    && let Line::HostCommand(command) = &line
                    && !matches!(command, HostCommand::SubmittedDraft { .. })
                {
                    let label = match command {
                        HostCommand::Action(action) => {
                            let current = status.lock().expect("surface status lock");
                            format!(
                                "/{}",
                                current
                                    .actions
                                    .as_ref()
                                    .expect("installed actions")
                                    .registry
                                    .help(*action)
                                    .name()
                            )
                        }
                        HostCommand::Tasks(_) => "/tasks".to_owned(),
                        HostCommand::SideQuestion(_) => "/side".to_owned(),
                        HostCommand::Config(_) => "/config".to_owned(),
                        HostCommand::Fork(_) => "/fork".to_owned(),
                        HostCommand::Resume(_) => "/resume".to_owned(),
                        HostCommand::History(_) => "/history".to_owned(),
                        _ => "local command".to_owned(),
                    };
                    let receipt = if matches!(command, HostCommand::Config(_)) {
                        "config write pending until the active turn finishes; saved defaults apply to future sessions".to_owned()
                    } else {
                        format!("{label} pending until the current turn ends")
                    };
                    commit_to_transcript(&status, &receipt);
                }
                if input.send(line).is_err() || (ended && failed) {
                    break;
                }
            }
        }
    }
}

/// Wait for a driver effect without taking the terminal away from the active
/// turn's permission/question modals or its bounded output pump.
#[allow(clippy::too_many_arguments)]
fn wait_image_reply(
    editor: &mut Reedline,
    phase: &SurfacePhase,
    surface: &mut ModalSurface<'_>,
    controls: &Receiver<Control>,
    pending_control: &mut Option<Control>,
    output: &Receiver<String>,
    signals: &Receiver<String>,
    answer: &Receiver<Result<Vec<ego_core::generator::types::ImageRef>, String>>,
) -> Option<Result<Vec<ego_core::generator::types::ImageRef>, String>> {
    loop {
        if let Some(control) = pending_control.take() {
            if !answer_control(editor, phase, surface, controls, pending_control, control) {
                return None;
            }
            continue;
        }
        crossbeam::channel::select_biased! {
            recv(controls) -> control => {
                let Ok(control) = control else { return None; };
                if matches!(control, Control::Shutdown) { return None; }
                if !answer_control(editor, phase, surface, controls, pending_control, control) { return None; }
            }
            recv(answer) -> reply => return Some(reply.unwrap_or_else(|_| Err("adding the image was cancelled; your draft was kept".to_owned()))),
            recv(signals) -> sequence => {
                let Ok(sequence) = sequence else { return None; };
                let mut out = std::io::stdout().lock();
                if let Err(error) = out.write_all(sequence.as_bytes()).and_then(|()| out.flush()) {
                    report_terminal_failure("output", error);
                    return None;
                }
            }
            recv(output) -> row => {
                let Ok(row) = row else { return None; };
                if let Err(error) = write_printer_message(&mut std::io::stdout().lock(), &row) {
                    report_terminal_failure("output", error);
                    return None;
                }
            }
        }
    }
}

/// Name the terminal failure that stops the terminal owner. Its exit ends the
/// interactive session, which would otherwise end with the cause unrecorded.
fn report_terminal_failure(what: &str, error: std::io::Error) {
    crate::output::note(&format!("terminal {what} failed: {error}"));
}

/// Write one external-printer message before Reedline reads, with the line
/// contract its own printer applies once it reads: `str::lines`, then CRLF.
/// Raw mode turns off output processing, so a bare LF would start the next
/// row at the column where this one ended. Splitting on lines rather than
/// replacing LF keeps an existing CRLF single and leaves control sequences
/// inside a line untouched.
fn write_printer_message(output: &mut impl std::io::Write, message: &str) -> std::io::Result<()> {
    for line in message.lines() {
        output.write_all(line.as_bytes())?;
        output.write_all(b"\r\n")?;
    }
    output.flush()
}

mod action_execution;
mod background_notice;
mod edit_mode;
mod permission;
mod permission_modal;
mod picker_surface;
mod question;
mod suspend;

#[cfg(test)]
use action_execution::apply_surface_effect;
pub(crate) use action_execution::{render_action_help, render_key_hints};
use action_execution::{request_interrupt, restore_latest_draft, surface_line};
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
    PermissionEditOutcome, Question, Waiting, answer_control, commit_to_transcript,
    commit_transcript_block, edit_permission_parameters, enter_modal, leave_modal,
    next_selector_key, parse_trust, question_selector_prompt, queued_selector_line,
};
#[cfg(test)]
use question::{PermissionWord, parse_permission};

#[cfg(test)]
mod tests;

mod session_picker;

// Only the isolated PTY child arms this seam. The real Seat::new printer and
// receipt sender are unchanged; the terminal owner cannot drain while held.
#[cfg(test)]
thread_local! {
    static HELD_PRINTER: std::cell::RefCell<Option<Sender<String>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn hold_permission_printer() {
    if std::env::var_os("EGO_HELD_PERMISSION_PTY_CHILD").is_none() {
        return;
    }
    HELD_PRINTER.with(|slot| {
        let slot = slot.borrow();
        let sender = slot.as_ref().expect("real live printer");
        assert_eq!(sender.capacity(), Some(1));
        sender
            .try_send("HELD-EXM-ROW\n".to_owned())
            .expect("empty modal printer");
    });
    std::fs::write("printer-held", "bounded(1) full").unwrap();
}

#[cfg(test)]
pub(super) fn await_permission_printer_release() {
    if std::env::var_os("EGO_HELD_PERMISSION_PTY_CHILD").is_some() {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !Path::new("printer-release").exists() {
            assert!(Instant::now() < deadline, "PTY printer release deadline");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(test)]
pub(crate) use question::test_resumed_round_terminal;
