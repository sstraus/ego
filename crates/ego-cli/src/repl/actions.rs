//! The single source of truth for interactive actions.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
    Clear,
    Usage,
    Statusline,
    Export,
    Doctor,
    Quit,
    New,
    Name,
    Goal,
    Tools,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Action(ActionId),
    Prompt,
    Immediate,
    InteractiveIngress,
    Refused { reason: &'static str },
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
    kind: ActionKind,
    availability: Availability,
}

impl ActionRegistration {
    pub fn new(id: ActionId, name: &'static str, kind: ActionKind) -> Self {
        Self {
            id,
            name,
            description: "",
            aliases: Vec::new(),
            shortcuts: Vec::new(),
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError(&'static str);

#[derive(Debug, Clone)]
pub struct ActionRegistry {
    registrations: Vec<ActionRegistration>,
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
            enabled(Model, "model", "Choose the current model").shortcut('p'),
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
            enabled(Mcp, "mcp", "Inspect admitted MCP connections").unavailable("the MCP client"),
            enabled(Sessions, "sessions", "Browse durable sessions")
                .unavailable("the session browser"),
            enabled(Fork, "fork", "Fork a durable session").unavailable("durable session replay"),
            enabled(Compact, "compact", "Compact the active context")
                .unavailable("the compaction service"),
            enabled(Clear, "clear", "Clear composer and menu state"),
            enabled(Usage, "usage", "Show usage, context, cost and budget")
                .unavailable("the usage inspector"),
            enabled(Statusline, "statusline", "Configure the native status line"),
            enabled(Export, "export", "Export sanitized session data")
                .unavailable("the session exporter"),
            enabled(Doctor, "doctor", "Diagnose local subsystem health")
                .unavailable("the diagnostics service"),
            enabled(Quit, "quit", "Exit the interactive session"),
            enabled(New, "new", "Create a durable session")
                .unavailable("the interactive session service"),
            enabled(Name, "name", "Set durable session display metadata")
                .unavailable("session metadata"),
            enabled(Goal, "goal", "Manage the persistent goal lifecycle")
                .unavailable("the goal service"),
            enabled(Tools, "tools", "Inspect available tools")
                .unavailable("the admitted tool registry"),
            enabled(Skills, "skills", "List or invoke admitted skills")
                .unavailable("the skill catalogue"),
            enabled(Hooks, "hooks", "Inspect admitted hooks").unavailable("the hook catalogue"),
            enabled(Version, "version", "Show packaged version information"),
        ])
        .expect("the final action surface is statically unique")
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
        self.registration(id).availability
    }
    pub fn kind(&self, id: ActionId) -> ActionKind {
        self.registration(id).kind
    }
    pub fn menu(&self, filter: &str) -> Vec<&ActionRegistration> {
        self.iter().filter(|a| a.name.starts_with(filter)).collect()
    }
    pub fn snapshot(&self) -> SurfaceSnapshot {
        SurfaceSnapshot {
            availability: self.iter().map(|a| (a.id, a.availability)).collect(),
        }
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
            || current_availability != registration.availability
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
        if submission.origin == InputOrigin::Typed {
            if let Some(name) = submission.text.strip_prefix('/') {
                if !name.contains(char::is_whitespace) {
                    if let Some(id) = self
                        .resolve_slash(name)
                        .or_else(|| self.resolve_alias(name))
                    {
                        return Dispatch::Action(id);
                    }
                }
            }
        }
        Dispatch::Prompt
    }
}
