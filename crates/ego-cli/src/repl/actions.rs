//! The single source of truth for interactive actions.

use std::collections::{HashMap, HashSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionId {
    Help, Model, Thinking, Mode, Plan, Yolo, Permissions, Mcp, Sessions, Fork,
    Compact, Clear, Usage, Statusline, Export, Doctor, Quit, New, Name, Goal,
    Tools, Skills, Hooks, Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind { Local, PromptProducing }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Enabled,
    Busy,
    Disabled { owner: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputOrigin { Typed, BracketedPaste }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission { text: String, origin: InputOrigin }

impl Submission {
    pub fn text(&self) -> &str { &self.text }
    pub fn origin(&self) -> InputOrigin { self.origin }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Action(ActionId),
    Prompt,
    Immediate,
    InteractiveIngress,
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
    handler: bool,
}

impl ActionRegistration {
    pub fn new(id: ActionId, name: &'static str, kind: ActionKind) -> Self {
        Self { id, name, description: "", aliases: Vec::new(), shortcuts: Vec::new(), kind,
            availability: Availability::Enabled, handler: true }
    }
    pub fn alias(mut self, alias: &'static str) -> Self { self.aliases.push(alias); self }
    pub fn shortcut(mut self, shortcut: char) -> Self { self.shortcuts.push(shortcut); self }
    pub fn description(mut self, description: &'static str) -> Self { self.description = description; self }
    pub fn unavailable(mut self, owner: &'static str) -> Self {
        self.availability = Availability::Disabled { owner };
        self.handler = false;
        self
    }
    pub fn id(&self) -> ActionId { self.id }
    pub fn name(&self) -> &str { self.name }
    pub fn description_text(&self) -> &str { self.description }
    pub fn aliases(&self) -> &[&'static str] { &self.aliases }
    pub fn shortcuts(&self) -> &[char] { &self.shortcuts }
    pub fn disabled_reason(&self) -> Option<&'static str> {
        match self.availability { Availability::Disabled { owner } => Some(owner), _ => None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceSnapshot { availability: HashMap<ActionId, Availability> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryError(&'static str);

#[derive(Debug, Clone)]
pub struct ActionRegistry { registrations: Vec<ActionRegistration> }

impl<const N: usize> TryFrom<[ActionRegistration; N]> for ActionRegistry {
    type Error = RegistryError;
    fn try_from(value: [ActionRegistration; N]) -> Result<Self, Self::Error> {
        let mut ids = HashSet::new();
        let mut names = HashSet::new();
        let mut aliases = HashSet::new();
        let mut shortcuts = HashSet::new();
        for action in &value {
            if !ids.insert(action.id) { return Err(RegistryError("duplicate action id")); }
            if !names.insert(action.name) { return Err(RegistryError("duplicate action name")); }
            for alias in &action.aliases {
                if !aliases.insert(*alias) || names.contains(alias) { return Err(RegistryError("duplicate action alias")); }
            }
            for shortcut in &action.shortcuts {
                if !shortcuts.insert(*shortcut) { return Err(RegistryError("duplicate action shortcut")); }
            }
        }
        // Also reject a canonical name which appeared earlier as an alias.
        if value.iter().any(|action| aliases.contains(action.name)) {
            return Err(RegistryError("name conflicts with action alias"));
        }
        Ok(Self { registrations: value.into() })
    }
}

impl ActionRegistry {
    pub fn final_surface() -> Self {
        use ActionId::*;
        use ActionKind::Local;
        let enabled = |id, name, description| ActionRegistration::new(id, name, Local).description(description);
        Self::try_from([
            enabled(Help, "help", "Show interactive actions and shortcuts"),
            enabled(Model, "model", "Choose the current model").shortcut('p'),
            enabled(Thinking, "thinking", "Choose reasoning effort"),
            enabled(Mode, "mode", "Inspect or change execution mode")
                .unavailable("the interactive mode controller"),
            enabled(Plan, "plan", "Enter plan mode")
                .unavailable("the interactive mode controller"),
            enabled(Yolo, "yolo", "Confirm relaxed execution policy")
                .unavailable("the interactive permission controller"),
            enabled(Permissions, "permissions", "Inspect effective permission rules")
                .unavailable("the interactive permission controller"),
            enabled(Mcp, "mcp", "Inspect admitted MCP connections").unavailable("the MCP client"),
            enabled(Sessions, "sessions", "Browse durable sessions").unavailable("the session browser"),
            enabled(Fork, "fork", "Fork a durable session").unavailable("durable session replay"),
            enabled(Compact, "compact", "Compact the active context")
                .unavailable("the compaction service"),
            enabled(Clear, "clear", "Clear composer and menu state"),
            enabled(Usage, "usage", "Show usage, context, cost and budget").unavailable("the usage inspector"),
            enabled(Statusline, "statusline", "Configure the native status line"),
            enabled(Export, "export", "Export sanitized session data").unavailable("the session exporter"),
            enabled(Doctor, "doctor", "Diagnose local subsystem health").unavailable("the diagnostics service"),
            enabled(Quit, "quit", "Exit the interactive session"),
            enabled(New, "new", "Create a durable session").unavailable("the interactive session service"),
            enabled(Name, "name", "Set durable session display metadata").unavailable("session metadata"),
            enabled(Goal, "goal", "Manage the persistent goal lifecycle").unavailable("the goal service"),
            enabled(Tools, "tools", "Inspect available tools").unavailable("the admitted tool registry"),
            enabled(Skills, "skills", "List or invoke admitted skills").unavailable("the skill catalogue"),
            enabled(Hooks, "hooks", "Inspect admitted hooks").unavailable("the hook catalogue"),
            enabled(Version, "version", "Show packaged version information"),
        ]).expect("the final action surface is statically unique")
    }

    pub fn len(&self) -> usize { self.registrations.len() }
    pub fn is_empty(&self) -> bool { self.registrations.is_empty() }
    pub fn iter(&self) -> impl Iterator<Item = &ActionRegistration> { self.registrations.iter() }
    pub fn names(&self) -> Vec<&str> { self.iter().map(ActionRegistration::name).collect() }
    pub fn id(&self, name: &str) -> Option<ActionId> { self.resolve_slash(name) }
    pub fn resolve_slash(&self, name: &str) -> Option<ActionId> {
        let name = name.strip_prefix('/').unwrap_or(name);
        self.iter().find(|a| a.name == name).map(|a| a.id)
    }
    pub fn resolve_alias(&self, alias: &str) -> Option<ActionId> {
        self.iter().find(|a| a.aliases.contains(&alias)).map(|a| a.id)
    }
    pub fn resolve_shortcut(&self, shortcut: char) -> Option<ActionId> {
        self.iter().find(|a| a.shortcuts.contains(&shortcut)).map(|a| a.id)
    }
    pub fn resolve_key(&self, key: KeyEvent) -> Option<ActionId> {
        match (key.code, key.modifiers) {
            (KeyCode::Char(character), modifiers) if modifiers == KeyModifiers::META => self.resolve_shortcut(character),
            _ => None,
        }
    }
    fn registration(&self, id: ActionId) -> &ActionRegistration {
        self.iter().find(|a| a.id == id).expect("registered action id")
    }
    pub fn help(&self, id: ActionId) -> &ActionRegistration { self.registration(id) }
    pub fn completion(&self, id: ActionId) -> &ActionRegistration { self.registration(id) }
    pub fn dispatch_projection(&self, id: ActionId) -> &ActionRegistration { self.registration(id) }
    pub fn availability(&self, id: ActionId) -> Availability { self.registration(id).availability }
    pub fn kind(&self, id: ActionId) -> ActionKind { self.registration(id).kind }
    pub fn handler(&self, id: ActionId) -> Option<ActionId> { self.registration(id).handler.then_some(id) }
    pub fn menu(&self, filter: &str) -> Vec<&ActionRegistration> {
        self.iter().filter(|a| a.name.starts_with(filter)).collect()
    }
    pub fn snapshot(&self) -> SurfaceSnapshot {
        SurfaceSnapshot { availability: self.iter().map(|a| (a.id, a.availability)).collect() }
    }
    pub fn with_availability(mut self, id: ActionId, availability: Availability) -> Self {
        let action = self.registrations.iter_mut().find(|a| a.id == id).expect("registered action id");
        action.availability = availability;
        self
    }
    pub fn dispatch(&self, id: ActionId, displayed: &SurfaceSnapshot) -> Dispatch {
        let current = self.availability(id);
        if displayed.availability.get(&id) != Some(&current) || current != Availability::Enabled || self.handler(id).is_none() {
            Dispatch::Refused { reason: "action is no longer available" }
        } else { self.route(id, false) }
    }
    pub fn route(&self, id: ActionId, _busy: bool) -> Dispatch {
        match self.kind(id) { ActionKind::Local => Dispatch::Immediate, ActionKind::PromptProducing => Dispatch::InteractiveIngress }
    }
    pub fn submit(&self, text: impl Into<String>, origin: InputOrigin) -> Submission { Submission { text: text.into(), origin } }
    pub fn classify(&self, submission: Submission) -> Dispatch {
        if submission.origin == InputOrigin::Typed {
            if let Some(name) = submission.text.strip_prefix('/') {
                if !name.contains(char::is_whitespace) {
                    if let Some(id) = self.resolve_slash(name).or_else(|| self.resolve_alias(name)) { return Dispatch::Action(id); }
                }
            }
        }
        Dispatch::Prompt
    }
}
