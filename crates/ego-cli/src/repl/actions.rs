//! The single source of truth for interactive actions.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ego_core::skill::AdmittedSkillOperation;

use crate::cli::{GoalOperation, NameOperation, SessionMetadataOperation};
use crate::skill::{SkillCatalogueCapture, SkillCatalogueView};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    Help,
    Model,
    Thinking,
    Mode,
    Plan,
    Yolo,
    Permissions,
    Mcp,
    Sessions,
    Fork,
    Compact,
    Refresh,
    Clear,
    Usage,
    Statusline,
    Export,
    Doctor,
    Config,
    Quit,
    New,
    Name,
    Goal,
    Tools,
    Expand,
    Skills,
    Hooks,
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Local,
    PromptProducing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Enabled,
    Busy,
    Disabled { owner: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOrigin {
    Typed,
    BracketedPaste,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    text: String,
    origin: InputOrigin,
}

impl Submission {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn origin(&self) -> InputOrigin {
        self.origin
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    Action(ActionId),
    SessionMetadata(SessionMetadataOperation),
    Skill(AdmittedSkillOperation),
    Skills(SkillSurfaceAction),
    Export(ExportSelection),
    Fork(ego_core::event::id::ItemId),
    Hooks,
    Config(String),
    Prompt,
    Immediate,
    InteractiveIngress,
    Refused { reason: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSurfaceAction {
    List,
    Diagnose,
    Rescan,
}

/// What `/export` was asked for, in the exporter's own terms.
///
/// The fields are exactly two of [`crate::cli::ExportArgs`]'s, and they are
/// carried rather than re-derived so the interactive surface cannot drift into
/// meaning something different by `json` than `ego export --format json` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportSelection {
    pub sanitize: bool,
    pub format: crate::cli::ExportFormat,
}

impl Default for ExportSelection {
    /// Sanitized markdown: see [`parse_export_action`] for why this is not the
    /// flag's default. This is also what the keyboard shortcut and the action
    /// menu select, since neither can carry a word.
    fn default() -> Self {
        Self {
            sanitize: true,
            format: crate::cli::ExportFormat::Md,
        }
    }
}

/// The action selected from one displayed registry snapshot.
///
/// Keeping the displayed availability with the identity prevents a menu that
/// has gone stale from silently authorising a different current action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionSelection {
    id: ActionId,
    displayed: Availability,
}

impl ActionSelection {
    pub fn id(&self) -> ActionId {
        self.id
    }
    pub fn displayed_availability(&self) -> Availability {
        self.displayed
    }
}

/// Everything a local handler needs from the routing layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalAction {
    pub id: ActionId,
    pub busy: bool,
}

/// A local action is single-flight: a second request observes the first one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalActionOutcome {
    Completed,
    InFlight,
    Refused { reason: &'static str },
}

/// The only callable boundary owned by the registry.
pub trait ActionSink {
    fn invoke(&mut self, action: LocalAction) -> LocalActionOutcome;
}

/// An adapter that lets a controller retain its identity while the composite
/// owns the callable action port.
pub struct SharedActionSink<T> {
    inner: Arc<Mutex<T>>,
}

impl<T> SharedActionSink<T> {
    pub fn new(inner: Arc<Mutex<T>>) -> Self {
        Self { inner }
    }

    pub fn inner(&self) -> Arc<Mutex<T>> {
        Arc::clone(&self.inner)
    }
}

impl<T: ActionSink> ActionSink for SharedActionSink<T> {
    fn invoke(&mut self, action: LocalAction) -> LocalActionOutcome {
        self.inner
            .lock()
            .expect("shared action sink lock")
            .invoke(action)
    }
}

/// The one process-local dispatcher for independently owned local actions.
///
/// Each stable action ID has at most one handler. Missing routes refuse by
/// name instead of silently treating an enabled registration as completed.
pub struct CompositeActionSink {
    routes: HashMap<ActionId, Box<dyn ActionSink + Send>>,
}

impl CompositeActionSink {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    pub fn register(
        &mut self,
        id: ActionId,
        sink: Box<dyn ActionSink + Send>,
    ) -> Result<(), CompositeActionSinkError> {
        if self.routes.contains_key(&id) {
            return Err(CompositeActionSinkError::Duplicate(id));
        }
        self.routes.insert(id, sink);
        Ok(())
    }

    pub fn handles(&self, id: ActionId) -> bool {
        self.routes.contains_key(&id)
    }

    pub fn validate_for(&self, registry: &ActionRegistry) -> Result<(), CompositeActionSinkError> {
        for registration in registry.iter() {
            if registry.availability(registration.id()) == Availability::Enabled
                && registry.kind(registration.id()) == ActionKind::Local
                && !self.handles(registration.id())
            {
                return Err(CompositeActionSinkError::Missing(registration.id()));
            }
        }
        Ok(())
    }
}

impl Default for CompositeActionSink {
    fn default() -> Self {
        Self::new()
    }
}

impl ActionSink for CompositeActionSink {
    fn invoke(&mut self, action: LocalAction) -> LocalActionOutcome {
        match self.routes.get_mut(&action.id) {
            Some(sink) => sink.invoke(action),
            None => LocalActionOutcome::Refused {
                reason: "the local action handler is not installed",
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeActionSinkError {
    Duplicate(ActionId),
    Missing(ActionId),
}

impl std::fmt::Display for CompositeActionSinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate(id) => write!(formatter, "action handler for {id:?} is duplicated"),
            Self::Missing(id) => write!(formatter, "action handler for {id:?} is missing"),
        }
    }
}

impl std::error::Error for CompositeActionSinkError {}

/// A prompt-producing action is data for the existing interactive ingress.
/// It is deliberately not a queue and cannot acknowledge durability itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptPayload {
    pub id: ActionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionDispatch {
    Local(LocalActionOutcome),
    Prompt(PromptPayload),
    Refused { reason: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRegistration {
    id: ActionId,
    name: &'static str,
    description: &'static str,
    aliases: Vec<&'static str>,
    shortcuts: Vec<char>,
    /// The word the banner puts after this action's shortcut; empty keeps the
    /// shortcut out of the banner and in the help.
    shortcut_hint: &'static str,
    kind: ActionKind,
    availability: Availability,
}

/// A key that is not a letter shortcut of an action: the editor and the run
/// loop handle it, and the registry is what says so to a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    label: &'static str,
    hint: &'static str,
    description: &'static str,
}

impl KeyBinding {
    /// `hint` is the banner word after `label`; empty keeps it out of the banner.
    pub fn new(label: &'static str, hint: &'static str, description: &'static str) -> Self {
        Self {
            label,
            hint,
            description,
        }
    }
}

/// One key as shown to a person, whether it came from a chord binding or from
/// an action's shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEntry {
    pub label: String,
    /// The banner phrase after the label; empty when the banner omits the key.
    pub hint: &'static str,
    pub description: &'static str,
}

impl ActionRegistration {
    pub fn new(id: ActionId, name: &'static str, kind: ActionKind) -> Self {
        Self {
            id,
            name,
            description: "",
            aliases: Vec::new(),
            shortcuts: Vec::new(),
            shortcut_hint: "",
            kind,
            availability: Availability::Enabled,
        }
    }
    pub fn alias(mut self, alias: &'static str) -> Self {
        self.aliases.push(alias);
        self
    }
    pub fn shortcut(mut self, shortcut: char) -> Self {
        self.shortcuts.push(shortcut);
        self
    }
    pub fn shortcut_hint(mut self, hint: &'static str) -> Self {
        self.shortcut_hint = hint;
        self
    }
    pub fn description(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }
    pub fn unavailable(mut self, owner: &'static str) -> Self {
        self.availability = Availability::Disabled { owner };
        self
    }
    pub fn id(&self) -> ActionId {
        self.id
    }
    pub fn name(&self) -> &str {
        self.name
    }
    pub fn description_text(&self) -> &str {
        self.description
    }
    pub fn aliases(&self) -> &[&'static str] {
        &self.aliases
    }
    pub fn shortcuts(&self) -> &[char] {
        &self.shortcuts
    }
    pub fn disabled_reason(&self) -> Option<&'static str> {
        match self.availability {
            Availability::Disabled { owner } => Some(owner),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceSnapshot {
    availability: HashMap<ActionId, Availability>,
    skill_catalogue_digest: Option<String>,
}

impl SurfaceSnapshot {
    pub fn skill_catalogue_digest(&self) -> Option<&str> {
        self.skill_catalogue_digest.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError(&'static str);

/// The inner sentence, and not `RegistryError("…")` around it.
///
/// The one thing this type holds is already a sentence written for a person,
/// so the only way to get it wrong was to print the wrapper too — which
/// `run.rs` did, with `{error:?}`, in a message an operator reads when their
/// slash commands failed to install.
impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for RegistryError {}

#[derive(Debug, Clone)]
pub struct ActionRegistry {
    registrations: Vec<ActionRegistration>,
    bindings: Vec<KeyBinding>,
    live_availability: Arc<Mutex<HashMap<ActionId, Availability>>>,
    skill_view: Arc<Mutex<Option<SkillCatalogueView>>>,
}

impl<const N: usize> TryFrom<[ActionRegistration; N]> for ActionRegistry {
    type Error = RegistryError;
    fn try_from(value: [ActionRegistration; N]) -> Result<Self, Self::Error> {
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let mut aliases = HashSet::new();
        let mut shortcuts = HashSet::new();
        for action in &value {
            if !ids.insert(action.id) {
                return Err(RegistryError("duplicate action id"));
            }
            if !names.insert(action.name) {
                return Err(RegistryError("duplicate action name"));
            }
            for alias in &action.aliases {
                if !aliases.insert(*alias) || names.contains(alias) {
                    return Err(RegistryError("duplicate action alias"));
                }
            }
            for shortcut in &action.shortcuts {
                if !shortcuts.insert(*shortcut) {
                    return Err(RegistryError("duplicate action shortcut"));
                }
            }
        }
        // Also reject a canonical name which appeared earlier as an alias.
        if value.iter().any(|action| aliases.contains(action.name)) {
            return Err(RegistryError("name conflicts with action alias"));
        }
        Ok(Self {
            registrations: value.into(),
            bindings: Vec::new(),
            live_availability: Arc::new(Mutex::new(HashMap::new())),
            skill_view: Arc::new(Mutex::new(None)),
        })
    }
}

impl ActionRegistry {
    pub fn final_surface() -> Self {
        use ActionId::*;
        use ActionKind::Local;
        let enabled = |id, name, description| {
            ActionRegistration::new(id, name, Local).description(description)
        };
        Self::try_from([
            enabled(Help, "help", "Show interactive actions and shortcuts"),
            enabled(Model, "model", "Choose the current model")
                .shortcut('p')
                .shortcut_hint("models"),
            enabled(Thinking, "thinking", "Choose reasoning effort"),
            enabled(Mode, "mode", "Inspect or change execution mode")
                .unavailable("the interactive mode controller"),
            enabled(Plan, "plan", "Enter plan mode").unavailable("the interactive mode controller"),
            enabled(Yolo, "yolo", "Confirm relaxed execution policy")
                .unavailable("the interactive permission controller"),
            enabled(
                Permissions,
                "permissions",
                "Inspect effective permission rules",
            )
            .unavailable("the interactive permission controller"),
            enabled(Mcp, "mcp", "Inspect admitted MCP connections"),
            enabled(Sessions, "sessions", "Browse durable sessions"),
            enabled(Fork, "fork", "Fork a durable session"),
            enabled(Compact, "compact", "Compact the active context")
                .unavailable("the compaction service"),
            enabled(Clear, "clear", "Clear composer and menu state"),
            enabled(Usage, "usage", "Show usage, context, cost and budget"),
            enabled(Statusline, "statusline", "Configure the native status line"),
            enabled(
                Export,
                "export",
                "Export this session — /export [local|sanitized] [md|json|jsonl]",
            ),
            enabled(
                Config,
                "config",
                "Inspect configuration — /config [perimeter | set KEY VALUE]",
            ),
            enabled(Doctor, "doctor", "Diagnose local subsystem health"),
            enabled(Quit, "quit", "Exit the interactive session"),
            enabled(
                Refresh,
                "refresh",
                "Apply current instructions in a new prompt epoch",
            ),
            enabled(New, "new", "Create a durable session"),
            enabled(Name, "name", "Set durable session display metadata"),
            enabled(Goal, "goal", "Manage the persistent goal lifecycle"),
            enabled(Tools, "tools", "Inspect available tools"),
            enabled(
                Expand,
                "expand",
                "Show the full output of the last tool call",
            )
            .shortcut('o'),
            enabled(Skills, "skills", "List or invoke admitted skills")
                .unavailable("the skill catalogue"),
            enabled(Hooks, "hooks", "Inspect admitted hooks"),
            enabled(Version, "version", "Show packaged version information"),
        ])
        .expect("the final action surface is statically unique")
        .with_binding(KeyBinding::new(
            "Esc",
            "interrupts",
            "Interrupt the running turn",
        ))
        .with_binding(KeyBinding::new(
            "Enter",
            "queues",
            "While a turn is running, queue the line for the next safe boundary",
        ))
        .with_binding(KeyBinding::new(
            "Ctrl+S",
            "draft",
            "Save the composer draft, or restore the last saved one",
        ))
        .with_binding(KeyBinding::new(
            "Ctrl+C",
            "",
            "Cancel a running turn; idle, abandon the line; twice in a row, exit",
        ))
    }

    /// Add a key the registry names but does not dispatch.
    pub fn with_binding(mut self, binding: KeyBinding) -> Self {
        self.bindings.push(binding);
        self
    }

    /// Every key, chords first and then the action shortcuts, in the one order
    /// the banner and the help both print.
    pub fn keys(&self) -> Vec<KeyEntry> {
        let chords = self.bindings.iter().map(|binding| KeyEntry {
            label: binding.label.to_owned(),
            hint: binding.hint,
            description: binding.description,
        });
        let shortcuts = self.iter().flat_map(|action| {
            action.shortcuts.iter().map(|shortcut| KeyEntry {
                label: format!("Option+{}", shortcut.to_ascii_uppercase()),
                hint: action.shortcut_hint,
                description: action.description,
            })
        });
        chords.chain(shortcuts).collect()
    }

    pub fn len(&self) -> usize {
        self.registrations.len()
    }
    pub fn is_empty(&self) -> bool {
        self.registrations.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = &ActionRegistration> {
        self.registrations.iter()
    }
    pub fn names(&self) -> Vec<&str> {
        self.iter().map(ActionRegistration::name).collect()
    }
    pub fn aliases(&self) -> Vec<&str> {
        self.iter()
            .flat_map(|registration| registration.aliases.iter().copied())
            .collect()
    }
    pub fn id(&self, name: &str) -> Option<ActionId> {
        self.resolve_slash(name)
    }
    pub fn resolve_slash(&self, name: &str) -> Option<ActionId> {
        let name = name.strip_prefix('/').unwrap_or(name);
        self.iter().find(|a| a.name == name).map(|a| a.id)
    }
    pub fn resolve_alias(&self, alias: &str) -> Option<ActionId> {
        self.iter()
            .find(|a| a.aliases.contains(&alias))
            .map(|a| a.id)
    }
    pub fn resolve_shortcut(&self, shortcut: char) -> Option<ActionId> {
        self.iter()
            .find(|a| a.shortcuts.contains(&shortcut))
            .map(|a| a.id)
    }
    pub fn resolve_key(&self, key: KeyEvent) -> Option<ActionId> {
        match (key.code, key.modifiers) {
            (KeyCode::Char('π'), KeyModifiers::NONE) => self.resolve_shortcut('p'),
            (KeyCode::Char(character), modifiers)
                if modifiers == KeyModifiers::ALT || modifiers == KeyModifiers::META =>
            {
                self.resolve_shortcut(character)
            }
            _ => None,
        }
    }
    fn registration(&self, id: ActionId) -> &ActionRegistration {
        self.iter()
            .find(|a| a.id == id)
            .expect("registered action id")
    }
    pub fn help(&self, id: ActionId) -> &ActionRegistration {
        self.registration(id)
    }
    pub fn completion(&self, id: ActionId) -> &ActionRegistration {
        self.registration(id)
    }
    pub fn dispatch_projection(&self, id: ActionId) -> &ActionRegistration {
        self.registration(id)
    }
    pub fn availability(&self, id: ActionId) -> Availability {
        self.live_availability
            .lock()
            .expect("action availability lock")
            .get(&id)
            .copied()
            .unwrap_or_else(|| self.registration(id).availability)
    }
    pub fn kind(&self, id: ActionId) -> ActionKind {
        self.registration(id).kind
    }
    pub fn menu(&self, filter: &str) -> Vec<&ActionRegistration> {
        self.iter().filter(|a| a.name.starts_with(filter)).collect()
    }
    pub fn snapshot(&self) -> SurfaceSnapshot {
        let skill_catalogue_digest = self
            .skill_capture()
            .ok()
            .flatten()
            .map(|capture| capture.snapshot_digest().to_owned());
        SurfaceSnapshot {
            availability: self
                .iter()
                .map(|a| (a.id, self.availability(a.id)))
                .collect(),
            skill_catalogue_digest,
        }
    }

    pub(crate) fn bind_skill_view(&self, view: SkillCatalogueView) -> Result<(), RegistryError> {
        let mut current = self.skill_view.lock().expect("skill view lock");
        if current.is_some() {
            return Err(RegistryError("the skill catalogue view is already bound"));
        }
        *current = Some(view);
        drop(current);
        self.set_availability(ActionId::Skills, Availability::Enabled);
        Ok(())
    }

    fn skill_capture(&self) -> Result<Option<SkillCatalogueCapture>, RegistryError> {
        let view = self.skill_view.lock().expect("skill view lock").clone();
        view.map(|view| {
            view.capture()
                .map_err(|_| RegistryError("the skill catalogue view is unavailable"))
        })
        .transpose()
    }

    pub(crate) fn skill_menu(&self, filter: &str) -> Vec<(String, String)> {
        let Ok(Some(capture)) = self.skill_capture() else {
            return Vec::new();
        };
        capture
            .descriptors()
            .iter()
            .filter(|spec| spec.descriptor().user_invocable && spec.name().starts_with(filter))
            .map(|spec| {
                (
                    spec.name().to_owned(),
                    spec.descriptor().description.clone(),
                )
            })
            .collect()
    }
    pub fn select(&self, id: ActionId, displayed: &SurfaceSnapshot) -> Option<ActionSelection> {
        displayed
            .availability
            .get(&id)
            .copied()
            .map(|availability| ActionSelection {
                id,
                displayed: availability,
            })
    }
    pub fn with_availability(mut self, id: ActionId, availability: Availability) -> Self {
        let action = self
            .registrations
            .iter_mut()
            .find(|a| a.id == id)
            .expect("registered action id");
        action.availability = availability;
        self
    }
    /// Change a live action without replacing the registry shared by the
    /// editor, completer, menu and dispatcher.
    pub fn set_availability(&self, id: ActionId, availability: Availability) {
        self.live_availability
            .lock()
            .expect("action availability lock")
            .insert(id, availability);
    }
    pub fn dispatch(&self, id: ActionId, displayed: &SurfaceSnapshot) -> Dispatch {
        let current = self.availability(id);
        if displayed.availability.get(&id) != Some(&current) || current != Availability::Enabled {
            Dispatch::Refused {
                reason: "action is no longer available",
            }
        } else {
            self.route(id, false)
        }
    }
    pub fn route(&self, id: ActionId, _busy: bool) -> Dispatch {
        match self.kind(id) {
            ActionKind::Local => Dispatch::Immediate,
            ActionKind::PromptProducing => Dispatch::InteractiveIngress,
        }
    }
    pub fn dispatch_to(
        &self,
        selection: ActionSelection,
        current: &SurfaceSnapshot,
        busy: bool,
        sink: &mut dyn ActionSink,
    ) -> ActionDispatch {
        let Some(current_availability) = current.availability.get(&selection.id).copied() else {
            return ActionDispatch::Refused {
                reason: "action is no longer available",
            };
        };
        let registration = self.registration(selection.id);
        if selection.displayed != current_availability
            || current_availability != self.availability(selection.id)
        {
            return ActionDispatch::Refused {
                reason: "action is no longer available",
            };
        }
        match current_availability {
            Availability::Disabled { owner } => {
                return ActionDispatch::Refused { reason: owner };
            }
            Availability::Busy => {
                return ActionDispatch::Refused {
                    reason: "action is no longer available",
                };
            }
            Availability::Enabled => {}
        }
        match registration.kind {
            ActionKind::Local => ActionDispatch::Local(sink.invoke(LocalAction {
                id: selection.id,
                busy,
            })),
            ActionKind::PromptProducing => {
                ActionDispatch::Prompt(PromptPayload { id: selection.id })
            }
        }
    }
    pub fn submit(&self, text: impl Into<String>, origin: InputOrigin) -> Submission {
        Submission {
            text: text.into(),
            origin,
        }
    }
    pub fn classify(&self, submission: Submission) -> Dispatch {
        if submission.origin == InputOrigin::Typed
            && let Some(command) = submission.text.strip_prefix('/')
        {
            let boundary = command.find(char::is_whitespace).unwrap_or(command.len());
            let name = &command[..boundary];
            let raw_arguments = command[boundary..].trim_start_matches(char::is_whitespace);
            if name == "name" && self.resolve_slash(name) == Some(ActionId::Name) {
                return parse_name_action(raw_arguments);
            }
            if name == "goal" && self.resolve_slash(name) == Some(ActionId::Goal) {
                return parse_goal_action(raw_arguments);
            }
            if name == "skills" {
                if self.resolve_slash(name) != Some(ActionId::Skills)
                    || self.availability(ActionId::Skills) != Availability::Enabled
                {
                    return Dispatch::Refused {
                        reason: "the skill catalogue is unavailable",
                    };
                }
                let capture = match self.skill_capture() {
                    Ok(Some(capture)) => capture,
                    Ok(None) | Err(_) => {
                        return Dispatch::Refused {
                            reason: "the skill catalogue is unavailable",
                        };
                    }
                };
                return match raw_arguments {
                    "" | "list" => Dispatch::Skills(SkillSurfaceAction::List),
                    "diagnose" => Dispatch::Skills(SkillSurfaceAction::Diagnose),
                    "rescan" => Dispatch::Skills(SkillSurfaceAction::Rescan),
                    arguments => match parse_skill_invoke(arguments) {
                        Some((skill_name, raw_arguments)) => {
                            match admitted_skill_operation(&capture, skill_name, raw_arguments) {
                                Some(operation) => Dispatch::Skill(operation),
                                None => Dispatch::Refused {
                                    reason: "the requested skill is not admitted for direct invocation",
                                },
                            }
                        }
                        None => Dispatch::Refused {
                            reason: "usage: /skills [list|diagnose|rescan|invoke <name> [arguments]]",
                        },
                    },
                };
            }
            if name == "fork"
                && self.resolve_slash(name) == Some(ActionId::Fork)
                && !raw_arguments.is_empty()
            {
                return match raw_arguments.parse::<ego_core::event::id::ItemId>() {
                    Ok(item) => Dispatch::Fork(item),
                    Err(_) => Dispatch::Refused {
                        reason: "usage: /fork [message-id]",
                    },
                };
            }
            if name == "export" && self.resolve_slash(name) == Some(ActionId::Export) {
                return if self.availability(ActionId::Export) == Availability::Enabled {
                    parse_export_action(raw_arguments)
                } else {
                    Dispatch::Refused {
                        reason: "export is unavailable",
                    }
                };
            }
            if name == "config" && self.resolve_slash(name) == Some(ActionId::Config) {
                if self.availability(ActionId::Config) != Availability::Enabled {
                    return Dispatch::Refused {
                        reason: "config is unavailable",
                    };
                }
                let arguments = raw_arguments.trim();
                return if arguments.is_empty() {
                    Dispatch::Action(ActionId::Config)
                } else if arguments == "perimeter" || arguments.starts_with("set ") {
                    Dispatch::Config(arguments.to_owned())
                } else {
                    Dispatch::Refused {
                        reason: "usage: /config [perimeter | set KEY TOML_VALUE]",
                    }
                };
            }
            if name == "hooks" && self.resolve_slash(name) == Some(ActionId::Hooks) {
                return if !raw_arguments.is_empty() {
                    Dispatch::Refused {
                        reason: "usage: /hooks",
                    }
                } else if self.availability(ActionId::Hooks) == Availability::Enabled {
                    Dispatch::Hooks
                } else {
                    Dispatch::Refused {
                        reason: "the hook catalogue is unavailable",
                    }
                };
            }
            let policy_action = self
                .resolve_slash(name)
                .or_else(|| self.resolve_alias(name))
                .filter(|id| {
                    matches!(
                        id,
                        ActionId::Mode | ActionId::Plan | ActionId::Yolo | ActionId::Permissions
                    )
                });
            if let Some(id) = policy_action {
                return if raw_arguments.is_empty() {
                    Dispatch::Action(id)
                } else {
                    Dispatch::Refused {
                        reason: "usage: /mode, /plan, /yolo, and /permissions take no arguments",
                    }
                };
            }
            if let Some(id) = self
                .resolve_slash(name)
                .or_else(|| self.resolve_alias(name))
            {
                return if boundary == command.len() {
                    Dispatch::Action(id)
                } else if id == ActionId::Mcp {
                    Dispatch::Refused {
                        reason: "usage: /mcp (inspect only); add servers with `ego mcp add NAME ...`",
                    }
                } else {
                    Dispatch::Refused {
                        reason: "this interactive action takes no arguments; use /help for local syntax",
                    }
                };
            }
            let capture = match self.skill_capture() {
                Ok(Some(capture)) => capture,
                Ok(None) => return Dispatch::Prompt,
                Err(_) => {
                    return Dispatch::Refused {
                        reason: "the skill catalogue view is unavailable",
                    };
                }
            };
            if let Some(operation) = admitted_skill_operation(&capture, name, raw_arguments) {
                return Dispatch::Skill(operation);
            }
        }
        Dispatch::Prompt
    }
}

/// `/export [local|sanitized] [md|json|jsonl]`, in either order.
///
/// The two disclosure words are `CLI.md`'s own — line 74 describes `ego export`
/// as rendering "one local or structurally sanitized transcript" — so the slash
/// grammar names the same two things the flag does rather than inventing a
/// third vocabulary for one exporter.
///
/// **The default differs from the flag's, deliberately.** `ego export` writes a
/// file the operator named and defaults to a local transcript; `/export` writes
/// to the *transcript on screen*, which is a place a bundle can be read over a
/// shoulder or captured by a recorder, so it defaults to sanitized and makes
/// `local` the word you type when you mean it. Same exporter, same options,
/// different safe default for a different destination.
fn parse_export_action(arguments: &str) -> Dispatch {
    const USAGE: &str = "usage: /export [local|sanitized] [md|json|jsonl]";

    let mut selection = ExportSelection::default();
    let (mut disclosure, mut format) = (false, false);
    for word in arguments.split_whitespace() {
        let claimed = match word {
            "local" | "sanitized" => std::mem::replace(&mut disclosure, true),
            "md" | "json" | "jsonl" => std::mem::replace(&mut format, true),
            _ => return Dispatch::Refused { reason: USAGE },
        };
        // Saying the same thing twice is a typo, and the second word silently
        // winning is how an operator gets `local` when they typed `sanitized`.
        if claimed {
            return Dispatch::Refused { reason: USAGE };
        }
        match word {
            "local" => selection.sanitize = false,
            "sanitized" => selection.sanitize = true,
            "md" => selection.format = crate::cli::ExportFormat::Md,
            "json" => selection.format = crate::cli::ExportFormat::Json,
            "jsonl" => selection.format = crate::cli::ExportFormat::Jsonl,
            _ => unreachable!("every accepted word is matched above"),
        }
    }
    Dispatch::Export(selection)
}

fn parse_name_action(arguments: &str) -> Dispatch {
    let operation = match arguments {
        "" | "show" => NameOperation::Show,
        "clear" => NameOperation::Clear,
        _ => match arguments
            .strip_prefix("set")
            .and_then(|tail| tail.strip_prefix(char::is_whitespace))
            .map(|tail| tail.trim_start_matches(char::is_whitespace))
            .filter(|text| !text.is_empty())
        {
            Some(text) => NameOperation::Set(text.to_owned()),
            None => {
                return Dispatch::Refused {
                    reason: "usage: /name [show|set TEXT|clear]",
                };
            }
        },
    };
    Dispatch::SessionMetadata(SessionMetadataOperation::Name(operation))
}

fn parse_goal_action(arguments: &str) -> Dispatch {
    let operation = match arguments {
        "" | "show" => GoalOperation::Show,
        "achieve" => GoalOperation::Achieve,
        _ => {
            let (keyword, operation) = if let Some(tail) = arguments.strip_prefix("set") {
                (tail, GoalOperation::Set as fn(String) -> GoalOperation)
            } else if let Some(tail) = arguments.strip_prefix("block") {
                (tail, GoalOperation::Block as fn(String) -> GoalOperation)
            } else {
                return Dispatch::Refused {
                    reason: "usage: /goal [show|set TEXT|achieve|block REASON]",
                };
            };
            match keyword
                .strip_prefix(char::is_whitespace)
                .map(|tail| tail.trim_start_matches(char::is_whitespace))
                .filter(|text| !text.is_empty())
            {
                Some(text) => operation(text.to_owned()),
                None => {
                    return Dispatch::Refused {
                        reason: "usage: /goal [show|set TEXT|achieve|block REASON]",
                    };
                }
            }
        }
    };
    Dispatch::SessionMetadata(SessionMetadataOperation::Goal(operation))
}

fn parse_skill_invoke(arguments: &str) -> Option<(&str, &str)> {
    let tail = arguments.strip_prefix("invoke")?;
    if !tail.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    let invocation = tail.trim_start_matches(char::is_whitespace);
    if invocation.is_empty() {
        return None;
    }
    let boundary = invocation
        .find(char::is_whitespace)
        .unwrap_or(invocation.len());
    Some((
        &invocation[..boundary],
        invocation[boundary..].trim_start_matches(char::is_whitespace),
    ))
}

fn admitted_skill_operation(
    capture: &SkillCatalogueCapture,
    name: &str,
    raw_arguments: &str,
) -> Option<AdmittedSkillOperation> {
    capture
        .descriptor(name)
        .filter(|spec| spec.descriptor().user_invocable)
        .map(|spec| AdmittedSkillOperation::invoke(spec.descriptor(), raw_arguments))
}

#[cfg(test)]
mod export_grammar {
    use super::*;
    use crate::cli::ExportFormat;

    fn selection(arguments: &str) -> ExportSelection {
        match parse_export_action(arguments) {
            Dispatch::Export(selection) => selection,
            other => panic!("`/export {arguments}` should have been accepted: {other:?}"),
        }
    }

    fn refusal(arguments: &str) -> &'static str {
        match parse_export_action(arguments) {
            Dispatch::Refused { reason } => reason,
            other => panic!("`/export {arguments}` should have been refused: {other:?}"),
        }
    }

    /// Bare `/export` is the safe rendering, because its destination is a
    /// transcript on a screen rather than a file the operator chose.
    #[test]
    fn the_default_is_sanitized_markdown() {
        assert_eq!(
            selection(""),
            ExportSelection {
                sanitize: true,
                format: ExportFormat::Md
            }
        );
        assert_eq!(selection(""), ExportSelection::default());
    }

    /// Both axes, independently, and in whichever order they were typed —
    /// an operator should not have to remember which word comes first.
    #[test]
    fn each_word_sets_its_own_axis_in_either_order() {
        for arguments in ["local jsonl", "jsonl local"] {
            assert_eq!(
                selection(arguments),
                ExportSelection {
                    sanitize: false,
                    format: ExportFormat::Jsonl
                },
                "`/export {arguments}` names one disclosure and one format"
            );
        }
        assert!(
            selection("json").sanitize,
            "a format alone changes no disclosure"
        );
        assert_eq!(selection("json").format, ExportFormat::Json);
        assert_eq!(
            selection("local").format,
            ExportFormat::Md,
            "a disclosure alone changes no format"
        );
        assert!(!selection("local").sanitize);
        assert!(
            selection("sanitized").sanitize,
            "the safe word is sayable, not only implied"
        );
    }

    /// The failure this guards is the quiet one: `/export sanitized local`
    /// silently exporting a *local* bundle because the last word won.
    #[test]
    fn a_repeated_axis_is_refused_rather_than_letting_the_last_word_win() {
        for arguments in ["sanitized local", "local sanitized", "md json", "json json"] {
            assert!(
                refusal(arguments).contains("usage:"),
                "`/export {arguments}` says one thing twice and must be refused"
            );
        }
    }

    /// An unknown word is never a prompt and never a silent default: an
    /// operator who typed `/export markdown` must be told, not handed a
    /// bundle in a format they did not ask for.
    #[test]
    fn an_unknown_word_is_refused_and_the_usage_names_every_option() {
        let reason = refusal("markdown");
        for option in ["local", "sanitized", "md", "json", "jsonl"] {
            assert!(
                reason.contains(option),
                "the usage line must name `{option}`: {reason}"
            );
        }
        assert!(refusal("--format json").contains("usage:"));
        assert!(refusal("local extra").contains("usage:"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{SkillCatalogue, SkillDiscovery};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    struct Roots {
        _dir: TempDir,
        system: PathBuf,
        user: PathBuf,
        project: PathBuf,
    }

    impl Roots {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("skill roots");
            let system = dir.path().join("etc/ego/skills");
            let user = dir.path().join("home/.ego/skills");
            let project = dir.path().join("workspace/.ego/skills");
            for root in [&system, &user, &project] {
                fs::create_dir_all(root).expect("create skill root");
            }
            Self {
                _dir: dir,
                system,
                user,
                project,
            }
        }

        fn write(&self, root: &Path, name: &str, frontmatter: &str, body: &str) {
            let directory = root.join(name);
            fs::create_dir_all(&directory).expect("create skill directory");
            fs::write(
                directory.join("SKILL.md"),
                format!(
                    "---\nname: {name}\ndescription: Explain {name}.\n{frontmatter}\n---\n{body}\n"
                ),
            )
            .expect("write skill");
        }

        fn catalogue(&self) -> SkillCatalogue {
            SkillCatalogue::discover(
                SkillDiscovery::new(
                    self.system.clone(),
                    self.user.clone(),
                    self.project.clone(),
                    false,
                ),
                &["help", "skills", "hooks", "quit", "q"],
            )
            .expect("valid skill catalogue")
        }

        fn registry(&self) -> (ActionRegistry, SkillCatalogue) {
            let catalogue = self.catalogue();
            let registry = ActionRegistry::final_surface();
            registry
                .bind_skill_view(catalogue.view())
                .expect("bind the owning catalogue view");
            (registry, catalogue)
        }
    }

    #[test]
    fn typed_skill_and_strict_surface_keep_exact_operation_and_refuse_non_user_skills() {
        let roots = Roots::new();
        roots.write(&roots.user, "explain", "argument-hint: QUERY", "Explain.");
        roots.write(
            &roots.user,
            "model-only",
            "user-invocable: false",
            "Model only.",
        );
        let (registry, _catalogue) = roots.registry();

        let Dispatch::Skill(direct) = registry
            .classify(registry.submit("/explain sh -c 'touch /tmp/nope'", InputOrigin::Typed))
        else {
            panic!("typed slash action must produce one Skill operation")
        };
        let Dispatch::Skill(strict) = registry.classify(registry.submit(
            "/skills invoke explain sh -c 'touch /tmp/nope'",
            InputOrigin::Typed,
        )) else {
            panic!("strict skill invocation must produce one Skill operation")
        };
        assert_eq!(direct.name(), "explain");
        assert_eq!(direct.raw_arguments(), "sh -c 'touch /tmp/nope'");
        assert_eq!(direct.descriptor_id(), strict.descriptor_id());
        assert_eq!(direct.raw_arguments(), strict.raw_arguments());
        assert_eq!(
            direct.descriptor_id(),
            registry
                .skill_capture()
                .expect("capture")
                .expect("bound catalogue")
                .descriptor("explain")
                .expect("descriptor")
                .descriptor()
                .id
        );

        assert_eq!(
            registry.classify(registry.submit(
                "The model says /explain sh -c 'touch /tmp/nope'",
                InputOrigin::BracketedPaste,
            )),
            Dispatch::Prompt
        );
        for command in [
            "/skills",
            "/skills list",
            "/skills diagnose",
            "/skills rescan",
        ] {
            assert!(
                matches!(
                    registry.classify(registry.submit(command, InputOrigin::Typed)),
                    Dispatch::Skills(_)
                ),
                "{command} must stay on the owned skills surface"
            );
        }
        for command in [
            "/skills invoke",
            "/skills invoke missing tail",
            "/skills invoke model-only tail",
        ] {
            assert!(
                matches!(
                    registry.classify(registry.submit(command, InputOrigin::Typed)),
                    Dispatch::Refused { .. }
                ),
                "{command} must be a typed refusal"
            );
        }
    }

    #[test]
    fn malformed_hooks_command_refuses_without_becoming_a_prompt() {
        let roots = Roots::new();
        let (registry, _catalogue) = roots.registry();

        assert_eq!(
            registry.classify(registry.submit("/hooks", InputOrigin::Typed)),
            Dispatch::Hooks
        );
        assert!(matches!(
            registry.classify(registry.submit("/hooks extra", InputOrigin::Typed)),
            Dispatch::Refused { .. }
        ));
    }

    #[test]
    fn fixed_names_and_aliases_remain_actions_while_collisions_are_diagnostic() {
        let roots = Roots::new();
        for name in ["help", "skills", "hooks", "quit", "q"] {
            roots.write(&roots.user, name, "", "must not shadow an action");
        }
        let catalogue = roots.catalogue();
        let captured = catalogue.capture().expect("capture");
        assert!(captured.descriptors().is_empty());
        for name in ["help", "skills", "hooks", "quit", "q"] {
            assert!(
                captured
                    .diagnostics()
                    .iter()
                    .any(|diagnostic| diagnostic.name() == name && diagnostic.is_collision()),
                "collision for {name} must remain visible"
            );
        }
        let registry = ActionRegistry::final_surface();
        assert_eq!(registry.resolve_slash("help"), Some(ActionId::Help));
        assert_eq!(registry.resolve_slash("skills"), Some(ActionId::Skills));
        assert_eq!(registry.resolve_slash("hooks"), Some(ActionId::Hooks));
        assert_eq!(registry.resolve_slash("quit"), Some(ActionId::Quit));
        let custom = ActionRegistry::try_from([
            ActionRegistration::new(ActionId::Help, "help", ActionKind::Local).alias("q"),
            ActionRegistration::new(ActionId::Skills, "skills", ActionKind::Local),
        ])
        .expect("custom action registration");
        assert_eq!(custom.resolve_alias("q"), Some(ActionId::Help));
    }

    #[test]
    fn one_immutable_capture_drives_announcement_completion_and_authorization() {
        let roots = Roots::new();
        roots.write(&roots.user, "explain", "", "Explain.");
        let (registry, _catalogue) = roots.registry();
        let captured = registry
            .skill_capture()
            .expect("capture")
            .expect("bound catalogue");
        let snapshot = registry.snapshot();
        assert_eq!(
            snapshot.skill_catalogue_digest(),
            Some(captured.snapshot_digest())
        );
        assert_eq!(registry.skill_menu("exp").len(), 1);
        assert_eq!(captured.descriptor("explain").unwrap().name(), "explain");
        let captured_again = registry
            .skill_capture()
            .expect("capture")
            .expect("bound catalogue");
        assert!(Arc::ptr_eq(&captured.core(), &captured_again.core()));
    }
}
