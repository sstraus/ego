//! Typed hook catalogue admission and durable invocation policy.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::channel::oneshot;
use futures::future::join_all;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{ConfigLayer, Source, Sourced, Trust};
use crate::digest::sha256_hex;
use crate::event::domain::{DomainEvent, EventKind};
use crate::event::hook::{
    HookActivation, HookContentDigest, HookDecision, HookDescriptorEvidence, HookFailure,
    HookFeedback, HookId, HookInvocationId, HookLifecycle, HookOccurrenceKey, HookOccurrenceOwner,
    HookOutcome, HookPhase, HookPlan, HookResult, HookSourceAuthority, HookSourceLocation,
    HookSubject,
};
use crate::event::id::{
    EventId, GenerationId, ItemId, Producer, SessionId, SubagentInvocationId, TurnId,
};
use crate::event::skill::{SkillInvocationId, SkillSourceAuthority};
use crate::permit::rule::{ArgvToken, Rule, Segment};
use crate::state::SessionState;
use crate::turn::{CancelToken, SharedJournal, SharedJournalError};

const MAX_FEEDBACK_BYTES: usize = 4096;
const MAX_TIMEOUT_SECONDS: u64 = 3600;
const MAX_HOOK_SCALAR_BYTES: usize = 16 * 1024;
const MAX_HOOK_COLLECTION_ENTRIES: usize = 256;
const MAX_HOOK_WIRE_BYTES: usize = 64 * 1024;

/// One typed declaration shared by TOML and projected skill frontmatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfigEntry {
    event: HookLifecycle,
    #[serde(default)]
    matcher: Option<HookMatcher>,
    #[serde(default)]
    once: bool,
    #[serde(default)]
    handler: HookHandlerConfig,
    #[serde(skip)]
    source_location: Option<String>,
    #[serde(skip)]
    invocation: Option<SkillInvocationId>,
}

impl HookConfigEntry {
    pub fn new(
        event: HookLifecycle,
        matcher: Option<HookMatcher>,
        handler: HookHandlerConfig,
    ) -> Self {
        Self {
            event,
            matcher,
            once: false,
            handler,
            source_location: None,
            invocation: None,
        }
    }

    pub fn with_once(mut self, once: bool) -> Self {
        self.once = once;
        self
    }

    pub fn event(&self) -> &'static str {
        self.event.as_str()
    }

    pub fn lifecycle(&self) -> HookLifecycle {
        self.event
    }

    pub fn matcher(&self) -> Option<&HookMatcher> {
        self.matcher.as_ref()
    }

    pub fn handler(&self) -> &HookHandlerConfig {
        &self.handler
    }

    pub fn once(&self) -> bool {
        self.once
    }

    /// Bind a projected skill entry to its invocation-scoped source authority.
    pub fn bind_skill(
        mut self,
        source_location: impl Into<String>,
        invocation: SkillInvocationId,
    ) -> Self {
        self.source_location = Some(source_location.into());
        self.invocation = Some(invocation);
        self
    }

    pub fn source_location(&self) -> &str {
        self.source_location
            .as_deref()
            .expect("source_location is available only on a bound skill hook entry")
    }

    pub fn invocation(&self) -> SkillInvocationId {
        self.invocation
            .expect("invocation is available only on a bound skill hook entry")
    }

    fn canonical(&self) -> String {
        format!(
            "{}\n{}\n{}\n{:?}",
            self.event.as_str(),
            self.matcher
                .as_ref()
                .map_or_else(String::new, HookMatcher::canonical),
            self.once,
            self.handler
        )
    }
}

/// An event-native matcher. Permission expressions reuse the canonical Rule AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookMatcher {
    Permission(Rule),
    Exact(String),
}

impl HookMatcher {
    fn canonical(&self) -> String {
        match self {
            Self::Permission(rule) => format!("permission:{rule}"),
            Self::Exact(value) => format!("exact:{value}"),
        }
    }

    fn matches(&self, event: &HookEvent) -> bool {
        match self {
            Self::Permission(matcher) => event
                .permission_selector
                .as_ref()
                .is_some_and(|selector| permission_rule_covers(matcher, selector)),
            Self::Exact(matcher) => event.selector.as_deref() == Some(matcher.as_str()),
        }
    }
}

impl Serialize for HookMatcher {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Permission(rule) => serializer.collect_str(rule),
            Self::Exact(value) => serializer.serialize_str(value),
        }
    }
}

impl<'de> Deserialize<'de> for HookMatcher {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if let Ok(rule) = Rule::load(&value) {
            return Ok(Self::Permission(rule));
        }
        if value.is_empty()
            || value.len() > MAX_HOOK_SCALAR_BYTES
            || value.chars().any(char::is_control)
            || (value.starts_with('/') && value.ends_with('/'))
        {
            return Err(serde::de::Error::custom("invalid exact hook matcher"));
        }
        Ok(Self::Exact(value))
    }
}

/// The current closed handler configuration shared by all four adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookHandlerConfig {
    Command {
        command: String,
        #[serde(default)]
        args: Option<Vec<String>>,
        #[serde(default)]
        shell: Option<String>,
        #[serde(default, rename = "timeout")]
        timeout_seconds: Option<u64>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(default)]
        allowed_env_vars: Option<Vec<String>>,
        #[serde(default, rename = "timeout")]
        timeout_seconds: Option<u64>,
    },
    McpTool {
        server: String,
        tool: String,
        input: serde_json::Value,
        #[serde(default, rename = "timeout")]
        timeout_seconds: Option<u64>,
    },
    Prompt {
        prompt: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default, rename = "timeout")]
        timeout_seconds: Option<u64>,
    },
}

impl Default for HookHandlerConfig {
    fn default() -> Self {
        Self::Prompt {
            prompt: String::new(),
            model: None,
            timeout_seconds: None,
        }
    }
}

impl HookHandlerConfig {
    pub fn timeout_for(&self, lifecycle: HookLifecycle) -> Duration {
        let configured = match self {
            Self::Command {
                timeout_seconds, ..
            }
            | Self::Http {
                timeout_seconds, ..
            }
            | Self::McpTool {
                timeout_seconds, ..
            }
            | Self::Prompt {
                timeout_seconds, ..
            } => *timeout_seconds,
        };
        let default = match self {
            Self::Prompt { .. } => 30,
            _ if matches!(
                lifecycle,
                HookLifecycle::UserPromptSubmit
                    | HookLifecycle::PreModelSwitch
                    | HookLifecycle::PostModelSwitch
            ) =>
            {
                30
            }
            _ => 600,
        };
        Duration::from_secs(configured.unwrap_or(default))
    }

    fn validate(&self) -> Result<(), HookCatalogueError> {
        let timeout = match self {
            Self::Command {
                command,
                args,
                shell,
                timeout_seconds,
            } => {
                validate_scalar("command", command, false)?;
                if args.is_some() && shell.is_some() {
                    return Err(HookCatalogueError::invalid(
                        "args",
                        "command args and shell are mutually exclusive",
                    ));
                }
                if let Some(args) = args {
                    validate_strings("args", args)?;
                }
                if let Some(shell) = shell {
                    validate_scalar("shell", shell, false)?;
                    if !matches!(shell.as_str(), "bash" | "powershell") {
                        return Err(HookCatalogueError::invalid(
                            "shell",
                            "shell must be exactly bash or powershell",
                        ));
                    }
                }
                if command.trim().is_empty() {
                    return Err(HookCatalogueError::invalid(
                        "command",
                        "command handler owner is empty",
                    ));
                }
                *timeout_seconds
            }
            Self::Http {
                url,
                headers,
                allowed_env_vars,
                timeout_seconds,
            } => {
                validate_scalar("url", url, false)?;
                let parsed = url::Url::parse(url)
                    .map_err(|_| HookCatalogueError::invalid("url", "invalid HTTP URL"))?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
                    return Err(HookCatalogueError::invalid(
                        "url",
                        "hook URL must be absolute HTTP or HTTPS",
                    ));
                }
                if headers.len() > MAX_HOOK_COLLECTION_ENTRIES
                    || headers.iter().any(|(name, value)| {
                        name.trim().is_empty()
                            || name.len() > MAX_HOOK_SCALAR_BYTES
                            || value.len() > MAX_HOOK_SCALAR_BYTES
                            || name.chars().any(char::is_control)
                    })
                {
                    return Err(HookCatalogueError::invalid(
                        "headers",
                        "invalid header name",
                    ));
                }
                if let Some(allowed_env_vars) = allowed_env_vars {
                    validate_strings("allowed_env_vars", allowed_env_vars)?;
                    if let Some(name) = allowed_env_vars.iter().find(|name| !valid_env_name(name)) {
                        return Err(HookCatalogueError::invalid(
                            "allowed_env_vars",
                            format!("invalid environment variable `{name}`"),
                        ));
                    }
                }
                *timeout_seconds
            }
            Self::McpTool {
                server,
                tool,
                input,
                timeout_seconds,
            } => {
                validate_scalar("server", server, false)?;
                validate_scalar("tool", tool, false)?;
                if server.trim().is_empty() {
                    return Err(HookCatalogueError::invalid(
                        "server",
                        "MCP handler owner is empty",
                    ));
                }
                if tool.trim().is_empty() {
                    return Err(HookCatalogueError::invalid(
                        "tool",
                        "MCP handler tool is empty",
                    ));
                }
                if !input.is_object()
                    || match serde_json::to_vec(input) {
                        Ok(bytes) => bytes.len() > MAX_HOOK_WIRE_BYTES,
                        Err(_) => true,
                    }
                {
                    return Err(HookCatalogueError::invalid(
                        "input",
                        "MCP hook input must be a JSON object of at most 64 KiB",
                    ));
                }
                *timeout_seconds
            }
            Self::Prompt {
                prompt,
                model,
                timeout_seconds,
            } => {
                validate_scalar("prompt", prompt, false)?;
                if let Some(model) = model {
                    validate_scalar("model", model, false)?;
                }
                *timeout_seconds
            }
        };
        if timeout.is_some_and(|seconds| seconds == 0 || seconds > MAX_TIMEOUT_SECONDS) {
            return Err(HookCatalogueError::invalid(
                "timeout",
                format!("must be between 1 and {MAX_TIMEOUT_SECONDS} seconds"),
            ));
        }
        if match serde_json::to_vec(self) {
            Ok(bytes) => bytes.len() > MAX_HOOK_WIRE_BYTES,
            Err(_) => true,
        } {
            return Err(HookCatalogueError::invalid(
                "handler",
                "canonical handler configuration exceeds 64 KiB",
            ));
        }
        Ok(())
    }
}

fn validate_scalar(
    field: &'static str,
    value: &str,
    allow_empty: bool,
) -> Result<(), HookCatalogueError> {
    if (!allow_empty && value.is_empty()) || value.len() > MAX_HOOK_SCALAR_BYTES {
        return Err(HookCatalogueError::invalid(
            field,
            "configured scalar is empty or exceeds 16 KiB",
        ));
    }
    Ok(())
}

fn validate_strings(field: &'static str, values: &[String]) -> Result<(), HookCatalogueError> {
    if values.len() > MAX_HOOK_COLLECTION_ENTRIES
        || values
            .iter()
            .any(|value| value.len() > MAX_HOOK_SCALAR_BYTES)
    {
        return Err(HookCatalogueError::invalid(
            field,
            "configured collection exceeds its hook-domain bound",
        ));
    }
    Ok(())
}

fn valid_env_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// One catalogue authority. Skills contain typed entries, never YAML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookSource {
    System,
    User,
    Project,
    Skill {
        location: String,
        invocation: SkillInvocationId,
        authority: SkillSourceAuthority,
        entries: Vec<HookConfigEntry>,
    },
}

impl HookSource {
    pub fn skill_entries_with_authority(
        location: impl Into<String>,
        invocation: SkillInvocationId,
        authority: SkillSourceAuthority,
        entries: impl IntoIterator<Item = HookConfigEntry>,
    ) -> Self {
        let location = location.into();
        Self::Skill {
            entries: entries
                .into_iter()
                .map(|entry| entry.bind_skill(location.clone(), invocation))
                .collect(),
            location,
            invocation,
            authority,
        }
    }

    pub const fn is_skill(&self) -> bool {
        matches!(self, Self::Skill { .. })
    }

    fn label(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Project => "project",
            Self::Skill { .. } => "skill",
        }
    }

    fn authority(&self) -> HookSourceAuthority {
        match self {
            Self::System => HookSourceAuthority::System,
            Self::User => HookSourceAuthority::User,
            Self::Project => HookSourceAuthority::Project,
            Self::Skill { authority, .. } => match authority {
                SkillSourceAuthority::System => HookSourceAuthority::System,
                SkillSourceAuthority::User => HookSourceAuthority::User,
                SkillSourceAuthority::Project => HookSourceAuthority::Project,
            },
        }
    }

    fn skill_invocation(&self) -> Option<SkillInvocationId> {
        match self {
            Self::Skill { invocation, .. } => Some(*invocation),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookCatalogueError {
    UnsupportedEvent { exact_name: String },
    UnsupportedField { exact_name: String },
    Invalid { exact_name: String, message: String },
    DuplicateIdentity { exact_name: String },
}

impl HookCatalogueError {
    pub fn exact_name(&self) -> &str {
        match self {
            Self::UnsupportedEvent { exact_name }
            | Self::UnsupportedField { exact_name }
            | Self::Invalid { exact_name, .. }
            | Self::DuplicateIdentity { exact_name } => exact_name,
        }
    }

    pub const fn candidate_published(&self) -> bool {
        false
    }

    fn invalid(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invalid {
            exact_name: name.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for HookCatalogueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEvent { exact_name } => {
                write!(formatter, "unsupported hook event `{exact_name}`")
            }
            Self::UnsupportedField { exact_name } => {
                write!(formatter, "unsupported hook field `{exact_name}`")
            }
            Self::Invalid {
                exact_name,
                message,
            } => write!(formatter, "invalid hook `{exact_name}`: {message}"),
            Self::DuplicateIdentity { exact_name } => {
                write!(formatter, "duplicate hook identity `{exact_name}`")
            }
        }
    }
}

impl std::error::Error for HookCatalogueError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    entry: HookConfigEntry,
    source: HookSource,
    evidence: HookDescriptorEvidence,
    catalogue_order: u32,
}

impl HookSpec {
    pub fn from_typed(
        entry: HookConfigEntry,
        source: HookSource,
        location: &str,
        ordinal: u32,
    ) -> Result<Self, HookCatalogueError> {
        let content = entry.canonical();
        Self::from_source_content(entry, source, location, ordinal, content.as_bytes())
    }

    fn from_source_content(
        entry: HookConfigEntry,
        source: HookSource,
        location: &str,
        ordinal: u32,
        source_content: &[u8],
    ) -> Result<Self, HookCatalogueError> {
        entry.handler.validate()?;
        if entry.once && !source.is_skill() {
            return Err(HookCatalogueError::invalid(
                "once",
                "once is valid only for invocation-scoped skill hooks",
            ));
        }
        let location = HookSourceLocation::parse(location)
            .map_err(|error| HookCatalogueError::invalid("location", error.to_string()))?;
        let digest = HookContentDigest::parse(&sha256_hex(source_content))
            .map_err(|error| HookCatalogueError::invalid("digest", error.to_string()))?;
        let evidence = HookDescriptorEvidence::new(source.authority(), location, digest, ordinal);
        Ok(Self {
            entry,
            source,
            evidence,
            catalogue_order: ordinal,
        })
    }

    pub fn from_wire(event: &str, handler: &str, owner: &str) -> Result<Self, HookCatalogueError> {
        let event =
            HookLifecycle::parse(event).ok_or_else(|| HookCatalogueError::UnsupportedEvent {
                exact_name: event.to_owned(),
            })?;
        let handler = match handler {
            "command" => HookHandlerConfig::Command {
                command: owner.to_owned(),
                args: None,
                shell: None,
                timeout_seconds: None,
            },
            "prompt" => HookHandlerConfig::Prompt {
                prompt: owner.to_owned(),
                model: None,
                timeout_seconds: None,
            },
            "mcp_tool" => {
                let (server, tool) = owner.split_once('/').ok_or_else(|| {
                    HookCatalogueError::invalid("owner", "MCP owner must be server/tool")
                })?;
                HookHandlerConfig::McpTool {
                    server: server.to_owned(),
                    tool: tool.to_owned(),
                    input: serde_json::json!({}),
                    timeout_seconds: None,
                }
            }
            "http" => HookHandlerConfig::Http {
                url: owner.to_owned(),
                headers: Vec::new(),
                allowed_env_vars: None,
                timeout_seconds: None,
            },
            exact_name => {
                return Err(HookCatalogueError::UnsupportedField {
                    exact_name: exact_name.to_owned(),
                });
            }
        };
        Self::from_typed(
            HookConfigEntry::new(event, None, handler),
            HookSource::User,
            "hooks/wire.toml",
            0,
        )
    }

    pub fn from_fields<'a>(
        event: &str,
        fields: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self, HookCatalogueError> {
        if HookLifecycle::parse(event).is_none() {
            return Err(HookCatalogueError::UnsupportedEvent {
                exact_name: event.to_owned(),
            });
        }
        for (name, _) in fields {
            if !matches!(
                name,
                "matcher"
                    | "once"
                    | "type"
                    | "command"
                    | "args"
                    | "shell"
                    | "timeout"
                    | "url"
                    | "headers"
                    | "allowed_env_vars"
                    | "server"
                    | "tool"
                    | "input"
                    | "prompt"
                    | "model"
            ) {
                return Err(HookCatalogueError::UnsupportedField {
                    exact_name: name.to_owned(),
                });
            }
        }
        Err(HookCatalogueError::invalid(
            "handler",
            "complete typed handler is required",
        ))
    }

    pub fn id(&self) -> HookId {
        self.evidence.id()
    }
    pub fn source(&self) -> HookSource {
        self.source.clone()
    }
    pub fn location(&self) -> &HookSourceLocation {
        self.evidence.location()
    }
    pub fn content_digest(&self) -> HookContentDigest {
        self.evidence.digest()
    }
    pub fn ordinal(&self) -> Option<u32> {
        Some(self.evidence.ordinal())
    }
    pub fn event_name(&self) -> &'static str {
        self.entry.event.as_str()
    }
    pub fn handler(&self) -> &HookHandlerConfig {
        &self.entry.handler
    }
    pub fn once(&self) -> bool {
        self.entry.once
    }
    pub fn skill_invocation(&self) -> Option<SkillInvocationId> {
        self.source.skill_invocation()
    }

    pub fn matches(&self, event: &HookEvent) -> bool {
        self.entry.event == event.lifecycle
            && self
                .entry
                .matcher
                .as_ref()
                .is_none_or(|matcher| matcher.matches(event))
    }

    pub fn sort_key(&self) -> HookSortKey {
        let authority = match self.source {
            HookSource::System => 0,
            HookSource::User => 1,
            HookSource::Project => 2,
            HookSource::Skill { .. } => 3,
        };
        HookSortKey(
            authority,
            self.catalogue_order,
            self.evidence.ordinal(),
            self.id().to_string(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HookSortKey(u8, u32, u32, String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEvent {
    lifecycle: HookLifecycle,
    subject: HookSubject,
    selector: Option<String>,
    permission_selector: Option<Rule>,
}

impl HookEvent {
    pub fn session(lifecycle: HookLifecycle) -> Self {
        Self {
            lifecycle,
            subject: HookSubject::Session,
            selector: None,
            permission_selector: None,
        }
    }

    pub fn turn(lifecycle: HookLifecycle, subject: HookSubject) -> Self {
        Self {
            lifecycle,
            subject,
            selector: None,
            permission_selector: None,
        }
    }

    pub fn tool(
        lifecycle: HookLifecycle,
        subject: HookSubject,
        selector: impl Into<String>,
    ) -> Self {
        let selector = selector.into();
        let permission_selector = Rule::parse(&selector).ok();
        Self {
            lifecycle,
            subject,
            selector: Some(selector),
            permission_selector,
        }
    }

    pub fn lifecycle(&self) -> HookLifecycle {
        self.lifecycle
    }

    pub fn subject(&self) -> HookSubject {
        self.subject
    }
}

/// Runtime-only causal envelope for one lifecycle occurrence.
///
/// The durable event keeps these values in its ordinary envelope. This value
/// exists only to make the owner that reaches a hook supply them explicitly
/// before the journal assembles that event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCausality {
    owner: HookOccurrenceOwner,
    turn: TurnId,
    item: Option<ItemId>,
    parent: Option<EventId>,
}

impl HookCausality {
    pub fn session(session: SessionId, turn: TurnId) -> Self {
        Self::new(HookOccurrenceOwner::Session { session }, turn, None, None)
    }

    pub fn event(event: EventId, turn: TurnId, item: Option<ItemId>) -> Self {
        Self::new(
            HookOccurrenceOwner::Event { event },
            turn,
            item,
            Some(event),
        )
    }

    pub fn item(item: ItemId, turn: TurnId, parent: EventId) -> Self {
        Self::new(
            HookOccurrenceOwner::Item { item },
            turn,
            Some(item),
            Some(parent),
        )
    }

    pub fn generation(generation: GenerationId, turn: TurnId, parent: EventId) -> Self {
        Self::new(
            HookOccurrenceOwner::Generation { generation },
            turn,
            None,
            Some(parent),
        )
    }

    pub fn subagent(
        invocation: SubagentInvocationId,
        turn: TurnId,
        item: ItemId,
        parent: EventId,
    ) -> Self {
        Self::new(
            HookOccurrenceOwner::Subagent { invocation },
            turn,
            Some(item),
            Some(parent),
        )
    }

    pub fn request(request: crate::run::RequestId, turn: TurnId, parent: Option<EventId>) -> Self {
        Self::new(HookOccurrenceOwner::Request { request }, turn, None, parent)
    }

    pub fn continuation(
        id: impl Into<String>,
        turn: TurnId,
        item: ItemId,
        parent: EventId,
    ) -> Result<Self, HookRunError> {
        let key = HookOccurrenceKey::continuation(HookLifecycle::Elicitation, id)
            .map_err(|error| HookRunError::new(error.to_string()))?;
        Ok(Self::new(
            key.owner().clone(),
            turn,
            Some(item),
            Some(parent),
        ))
    }

    fn new(
        owner: HookOccurrenceOwner,
        turn: TurnId,
        item: Option<ItemId>,
        parent: Option<EventId>,
    ) -> Self {
        Self {
            owner,
            turn,
            item,
            parent,
        }
    }

    pub fn owner(&self) -> &HookOccurrenceOwner {
        &self.owner
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }

    pub fn item_id(&self) -> Option<ItemId> {
        self.item
    }

    pub fn parent(&self) -> Option<EventId> {
        self.parent
    }
}

/// The one cancellation and deadline authority shared by every effect of a
/// hook invocation. It is runtime state and is never serialized.
#[derive(Debug, Clone)]
pub struct EffectScope {
    cancel: CancelToken,
    deadline: Instant,
    timeout: Duration,
}

impl EffectScope {
    pub fn new(timeout: Duration) -> Self {
        Self::with_parent(CancelToken::new(), None, timeout)
    }

    pub fn with_parent(
        cancel: CancelToken,
        parent_deadline: Option<Instant>,
        timeout: Duration,
    ) -> Self {
        let configured = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        let deadline = parent_deadline.map_or(configured, |parent| parent.min(configured));
        Self {
            cancel,
            deadline,
            timeout,
        }
    }

    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// Runtime-only invocation material handed to exactly one handler adapter.
#[derive(Debug, Clone)]
pub struct HookInvocationRequest {
    started: HookStartedEvidence,
    causality: HookCausality,
    handler: HookHandlerConfig,
    event: HookEvent,
    effect_scope: EffectScope,
}

impl HookInvocationRequest {
    pub fn new(
        started: HookStartedEvidence,
        causality: HookCausality,
        handler: HookHandlerConfig,
        event: HookEvent,
        effect_scope: EffectScope,
    ) -> Self {
        Self {
            started,
            causality,
            handler,
            event,
            effect_scope,
        }
    }

    pub fn hook(&self) -> HookId {
        self.started.hook()
    }

    pub fn started(&self) -> HookStartedEvidence {
        self.started
    }

    pub fn causality(&self) -> &HookCausality {
        &self.causality
    }

    pub fn handler(&self) -> &HookHandlerConfig {
        &self.handler
    }

    pub fn lifecycle(&self) -> HookLifecycle {
        self.event.lifecycle
    }

    pub fn subject(&self) -> HookSubject {
        self.event.subject
    }

    pub fn selector(&self) -> Option<&str> {
        self.event.selector.as_deref()
    }

    pub fn activation(&self) -> HookActivation {
        self.started.activation()
    }

    pub fn invocation(&self) -> HookInvocationId {
        self.started.invocation()
    }

    pub fn effect_scope(&self) -> &EffectScope {
        &self.effect_scope
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HookPhaseV1 {
    PreEffect,
    Observer,
}

impl From<HookPhase> for HookPhaseV1 {
    fn from(value: HookPhase) -> Self {
        if value.is_pre_effect() {
            Self::PreEffect
        } else {
            Self::Observer
        }
    }
}

/// Strict versioned event envelope used inside MCP hook input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HookEventV1 {
    version: u8,
    hook_id: HookId,
    invocation_id: HookInvocationId,
    activation: HookActivation,
    event: HookLifecycle,
    phase: HookPhaseV1,
    subject: HookSubject,
    #[serde(skip_serializing_if = "Option::is_none")]
    selector: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookEventWire {
    version: u8,
    hook_id: HookId,
    invocation_id: HookInvocationId,
    activation: HookActivation,
    event: HookLifecycle,
    phase: HookPhaseV1,
    subject: HookSubject,
    #[serde(default)]
    selector: Option<String>,
}

impl<'de> Deserialize<'de> for HookEventV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = HookEventWire::deserialize(deserializer)?;
        if wire.version != 1 || wire.phase != HookPhaseV1::from(wire.event.phase()) {
            return Err(serde::de::Error::custom(
                "invalid hook event version or phase",
            ));
        }
        if wire
            .selector
            .as_ref()
            .is_some_and(|selector| selector.len() > MAX_HOOK_SCALAR_BYTES)
        {
            return Err(serde::de::Error::custom("hook selector exceeds 16 KiB"));
        }
        Ok(Self {
            version: wire.version,
            hook_id: wire.hook_id,
            invocation_id: wire.invocation_id,
            activation: wire.activation,
            event: wire.event,
            phase: wire.phase,
            subject: wire.subject,
            selector: wire.selector,
        })
    }
}

impl HookEventV1 {
    pub fn from_runtime(request: &HookInvocationRequest) -> Result<Self, HookWireError> {
        request
            .handler
            .validate()
            .map_err(HookWireError::catalogue)?;
        let value = Self {
            version: 1,
            hook_id: request.hook(),
            invocation_id: request.invocation(),
            activation: request.activation(),
            event: request.event.lifecycle,
            phase: request.event.lifecycle.phase().into(),
            subject: request.event.subject,
            selector: request.event.selector.clone(),
        };
        validate_wire_size(&value)?;
        Ok(value)
    }
}

/// Strict command-stdin and HTTP-body request. Runtime-only effect state and
/// handler configuration are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HookHandlerRequestV1(HookEventV1);

impl HookHandlerRequestV1 {
    pub fn from_runtime(request: &HookInvocationRequest) -> Result<Self, HookWireError> {
        Ok(Self(HookEventV1::from_runtime(request)?))
    }

    pub fn invocation_id(&self) -> HookInvocationId {
        self.0.invocation_id
    }

    pub fn activation(&self) -> HookActivation {
        self.0.activation
    }
}

/// The sole accepted raw handler settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HookHandlerResultV1 {
    version: u8,
    decision: HookDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    feedback: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookHandlerResultWire {
    version: u8,
    decision: HookDecision,
    #[serde(default)]
    feedback: Option<String>,
}

impl<'de> Deserialize<'de> for HookHandlerResultV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = HookHandlerResultWire::deserialize(deserializer)?;
        if wire.version != 1 {
            return Err(serde::de::Error::custom("unsupported hook result version"));
        }
        if wire
            .feedback
            .as_ref()
            .is_some_and(|feedback| feedback.len() > MAX_HOOK_WIRE_BYTES)
        {
            return Err(serde::de::Error::custom("hook feedback exceeds 64 KiB"));
        }
        Ok(Self {
            version: wire.version,
            decision: wire.decision,
            feedback: wire.feedback,
        })
    }
}

impl HookHandlerResultV1 {
    pub fn parse(bytes: &[u8]) -> Result<Self, HookWireError> {
        if bytes.len() > MAX_HOOK_WIRE_BYTES {
            return Err(HookWireError::new("hook result exceeds 64 KiB"));
        }
        let mut deserializer = serde_json::Deserializer::from_slice(bytes);
        let result = Self::deserialize(&mut deserializer)
            .map_err(|error| HookWireError::new(error.to_string()))?;
        deserializer
            .end()
            .map_err(|error| HookWireError::new(error.to_string()))?;
        Ok(result)
    }

    pub fn decision(&self) -> HookDecision {
        self.decision
    }

    pub fn feedback(&self) -> Option<&str> {
        self.feedback.as_deref()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookWireError(String);

impl HookWireError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    fn catalogue(error: HookCatalogueError) -> Self {
        Self(error.to_string())
    }
}

impl fmt::Display for HookWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for HookWireError {}

fn validate_wire_size(value: &impl Serialize) -> Result<(), HookWireError> {
    let bytes = serde_json::to_vec(value).map_err(|error| HookWireError::new(error.to_string()))?;
    if bytes.len() > MAX_HOOK_WIRE_BYTES {
        return Err(HookWireError::new("hook request exceeds 64 KiB"));
    }
    Ok(())
}

fn permission_rule_covers(matcher: &Rule, selector: &Rule) -> bool {
    match (matcher, selector) {
        (Rule::Read(pattern), Rule::Read(target))
        | (Rule::Write(pattern), Rule::Write(target))
        | (Rule::List(pattern), Rule::List(target))
        | (Rule::Search(pattern), Rule::Search(target)) => {
            if target.segments().iter().any(|segment| match segment {
                Segment::Any => true,
                Segment::One(glob) => glob.as_literal().is_none(),
            }) {
                return false;
            }
            pattern.matches(Path::new(&target.to_string()), Path::new(""))
        }
        (Rule::Exec(pattern), Rule::Exec(target)) => {
            let argv = target
                .tokens()
                .iter()
                .map(|token| match token {
                    ArgvToken::One(glob) => glob.as_literal().map(str::to_owned),
                    ArgvToken::AnyOne | ArgvToken::Rest => None,
                })
                .collect::<Option<Vec<_>>>();
            argv.is_some_and(|argv| pattern.matches(&argv))
        }
        (Rule::Net(_), Rule::Net(_)) | (Rule::Tool { .. }, Rule::Tool { .. }) => {
            matcher == selector
        }
        _ => false,
    }
}

/// A fully validated immutable publication candidate.
#[derive(Debug, Clone)]
pub struct HookCatalogueCandidate {
    specs: Vec<HookSpec>,
    sources: Vec<&'static str>,
}

impl HookCatalogueCandidate {
    pub fn from_sources(
        layers: &[Sourced<ConfigLayer>],
        skills: &[HookSource],
        trust: Trust,
    ) -> Result<Self, HookCatalogueError> {
        let mut specs = Vec::new();
        let mut sources = Vec::new();
        for layer in layers {
            match layer.source {
                Source::Project if trust != Trust::Trusted => {
                    return Err(HookCatalogueError::invalid(
                        "project",
                        "untrusted project hooks cannot be admitted",
                    ));
                }
                Source::System | Source::User | Source::Project => {}
                _ if layer.value.hooks.is_empty() => {}
                other => {
                    return Err(HookCatalogueError::invalid(
                        other.to_string(),
                        "source cannot own persistent hooks",
                    ));
                }
            }
        }
        for (layer_source, source) in [
            (Source::System, HookSource::System),
            (Source::User, HookSource::User),
            (Source::Project, HookSource::Project),
        ] {
            for layer in layers.iter().filter(|layer| layer.source == layer_source) {
                admit_source_entries(
                    &mut specs,
                    &mut sources,
                    source.clone(),
                    persistent_location(&source),
                    &layer.value.hooks,
                )?;
            }
        }
        for skill in skills {
            let HookSource::Skill {
                location,
                invocation,
                authority: _,
                entries,
            } = skill
            else {
                return Err(HookCatalogueError::invalid(
                    skill.label(),
                    "invocation source must be a skill",
                ));
            };
            if entries.iter().any(|entry| {
                entry.source_location.as_deref() != Some(location.as_str())
                    || entry.invocation != Some(*invocation)
            }) {
                return Err(HookCatalogueError::invalid(
                    "skill",
                    "skill hook entry is not bound to its source activation",
                ));
            }
            admit_source_entries(&mut specs, &mut sources, skill.clone(), location, entries)?;
        }
        let mut ids = HashSet::new();
        for (order, spec) in specs.iter_mut().enumerate() {
            if !ids.insert(spec.id()) {
                return Err(HookCatalogueError::DuplicateIdentity {
                    exact_name: spec.id().to_string(),
                });
            }
            spec.catalogue_order = u32::try_from(order)
                .map_err(|_| HookCatalogueError::invalid("catalogue", "too many hooks"))?;
        }
        Ok(Self { specs, sources })
    }
}

fn admit_source_entries(
    specs: &mut Vec<HookSpec>,
    sources: &mut Vec<&'static str>,
    source: HookSource,
    location: &str,
    entries: &[HookConfigEntry],
) -> Result<(), HookCatalogueError> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut identities = HashSet::new();
    for entry in entries {
        if !identities.insert(entry.canonical()) {
            return Err(HookCatalogueError::DuplicateIdentity {
                exact_name: entry.event.as_str().to_owned(),
            });
        }
    }
    if !sources.contains(&source.label()) {
        sources.push(source.label());
    }
    let content = entries
        .iter()
        .map(HookConfigEntry::canonical)
        .collect::<Vec<_>>()
        .join("\n--\n");
    for (ordinal, entry) in entries.iter().cloned().enumerate() {
        let ordinal = u32::try_from(ordinal)
            .map_err(|_| HookCatalogueError::invalid("ordinal", "too many hooks"))?;
        specs.push(HookSpec::from_source_content(
            entry,
            source.clone(),
            location,
            ordinal,
            content.as_bytes(),
        )?);
    }
    Ok(())
}

fn persistent_location(source: &HookSource) -> &'static str {
    match source {
        HookSource::System => "hooks/system.toml",
        HookSource::User => "hooks/user.toml",
        HookSource::Project => "hooks/project.toml",
        HookSource::Skill { .. } => unreachable!("skill location is supplied"),
    }
}

#[derive(Debug, Clone)]
pub struct HookCatalogue {
    specs: Vec<HookSpec>,
    sources: Vec<&'static str>,
    generation: String,
}

impl HookCatalogue {
    pub fn admit_typed(
        layers: &[Sourced<ConfigLayer>],
        skills: &[HookSource],
        trust: Trust,
    ) -> Result<Self, HookCatalogueError> {
        Ok(Self::publish(
            HookCatalogueCandidate::from_sources(layers, skills, trust)?,
            "initial",
        ))
    }

    fn publish(candidate: HookCatalogueCandidate, boundary: &str) -> Self {
        let material = candidate
            .specs
            .iter()
            .map(|spec| spec.id().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            specs: candidate.specs,
            sources: candidate.sources,
            generation: sha256_hex(format!("{boundary}\n{material}").as_bytes()),
        }
    }

    pub fn reload_typed(
        &mut self,
        layers: &[Sourced<ConfigLayer>],
        skills: &[HookSource],
        trust: Trust,
    ) -> Result<(), HookCatalogueError> {
        let candidate = HookCatalogueCandidate::from_sources(layers, skills, trust)?;
        let next = Self::publish(candidate, &self.generation);
        *self = next;
        Ok(())
    }

    pub fn specs(&self) -> &[HookSpec] {
        &self.specs
    }
    pub fn sources(&self) -> &[&'static str] {
        &self.sources
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn matching(&self, event: &HookEvent) -> Vec<&HookSpec> {
        let mut matching = self
            .specs
            .iter()
            .filter(|spec| spec.matches(event))
            .collect::<Vec<_>>();
        matching.sort_by_key(|spec| spec.sort_key());
        matching
    }
    pub fn serialized_config(&self) -> String {
        self.specs
            .iter()
            .map(|spec| spec.id().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// One lifecycle occurrence paired with the event material matched by hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOccurrence {
    key: HookOccurrenceKey,
    event: HookEvent,
    causality: HookCausality,
}

impl HookOccurrence {
    pub fn new(
        key: HookOccurrenceKey,
        event: HookEvent,
        causality: HookCausality,
    ) -> Result<Self, HookRunError> {
        if key.lifecycle() != event.lifecycle() {
            return Err(HookRunError::new(
                "hook occurrence lifecycle does not match its event",
            ));
        }
        if key.owner() != causality.owner() {
            return Err(HookRunError::new(
                "hook occurrence owner does not match its causality",
            ));
        }
        let subject_matches = match event.subject() {
            HookSubject::Session => causality.item_id().is_none(),
            HookSubject::Turn { turn } => turn == causality.turn() && causality.item_id().is_none(),
            HookSubject::Item { item } => causality.item_id() == Some(item),
        };
        if !subject_matches {
            return Err(HookRunError::new(
                "hook occurrence subject does not match its causality",
            ));
        }
        match key.owner() {
            HookOccurrenceOwner::Event { event } if causality.parent() != Some(*event) => {
                return Err(HookRunError::new(
                    "hook event owner is not its causal parent",
                ));
            }
            HookOccurrenceOwner::Item { item } if causality.item_id() != Some(*item) => {
                return Err(HookRunError::new("hook item owner is not its causal item"));
            }
            HookOccurrenceOwner::Continuation { .. }
                if causality.item_id().is_none() || causality.parent().is_none() =>
            {
                return Err(HookRunError::new(
                    "hook continuation has no tool item and parent",
                ));
            }
            HookOccurrenceOwner::Session { .. }
            | HookOccurrenceOwner::Event { .. }
            | HookOccurrenceOwner::Item { .. }
            | HookOccurrenceOwner::Generation { .. }
            | HookOccurrenceOwner::Subagent { .. }
            | HookOccurrenceOwner::Request { .. }
            | HookOccurrenceOwner::Continuation { .. } => {}
        }
        Ok(Self {
            key,
            event,
            causality,
        })
    }

    pub fn key(&self) -> &HookOccurrenceKey {
        &self.key
    }

    pub fn event(&self) -> &HookEvent {
        &self.event
    }

    pub fn causality(&self) -> &HookCausality {
        &self.causality
    }
}

/// Durable proof that a real owner froze one exact ordered hook plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookReachedEvidence {
    event: EventId,
    occurrence: HookOccurrenceKey,
    subject: HookSubject,
    producer: Producer,
    plan: HookPlan,
}

impl HookReachedEvidence {
    pub fn new(
        event: EventId,
        occurrence: HookOccurrenceKey,
        subject: HookSubject,
        producer: Producer,
        plan: HookPlan,
    ) -> Self {
        Self {
            event,
            occurrence,
            subject,
            producer,
            plan,
        }
    }

    pub fn event(&self) -> EventId {
        self.event
    }
    pub fn occurrence(&self) -> &HookOccurrenceKey {
        &self.occurrence
    }
    pub fn subject(&self) -> HookSubject {
        self.subject
    }
    pub fn producer(&self) -> &Producer {
        &self.producer
    }
    pub fn plan(&self) -> &HookPlan {
        &self.plan
    }
}

/// Durable evidence sufficient to bind a start and reconstruct skill-once use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HookStartedEvidence {
    event: EventId,
    occurrence: EventId,
    hook: HookId,
    invocation: HookInvocationId,
    activation: HookActivation,
}

impl HookStartedEvidence {
    pub fn new(
        event: EventId,
        occurrence: EventId,
        hook: HookId,
        invocation: HookInvocationId,
        activation: HookActivation,
    ) -> Self {
        Self {
            event,
            occurrence,
            hook,
            invocation,
            activation,
        }
    }

    pub fn event(&self) -> EventId {
        self.event
    }
    pub fn occurrence(&self) -> EventId {
        self.occurrence
    }
    pub fn hook(&self) -> HookId {
        self.hook
    }
    pub fn invocation(&self) -> HookInvocationId {
        self.invocation
    }
    pub fn activation(&self) -> HookActivation {
        self.activation
    }

    pub fn from_event(event: &DomainEvent) -> Option<Self> {
        match &event.kind {
            EventKind::HookInvocationStarted {
                occurrence,
                hook,
                invocation,
                activation,
                ..
            } => Some(Self::new(
                event.env.id,
                *occurrence,
                *hook,
                *invocation,
                *activation,
            )),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSettledEvidence {
    event: EventId,
    occurrence: EventId,
    hook: HookId,
    invocation: HookInvocationId,
    result: HookResult,
}

impl HookSettledEvidence {
    pub fn new(
        event: EventId,
        occurrence: EventId,
        hook: HookId,
        invocation: HookInvocationId,
        result: HookResult,
    ) -> Self {
        Self {
            event,
            occurrence,
            hook,
            invocation,
            result,
        }
    }
    pub fn event(&self) -> EventId {
        self.event
    }
    pub fn occurrence(&self) -> EventId {
        self.occurrence
    }
    pub fn hook(&self) -> HookId {
        self.hook
    }
    pub fn invocation(&self) -> HookInvocationId {
        self.invocation
    }
    pub fn result(&self) -> &HookResult {
        &self.result
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookReachOutcome {
    Fresh(HookReachedEvidence),
    Recovered {
        evidence: HookReachedEvidence,
        starts: Vec<HookStartedEvidence>,
        settlements: Vec<HookSettledEvidence>,
    },
}

pub type HookFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HookOutcome, HookFailure>> + Send + 'a>>;

/// Synchronous durable authority used by the static lifecycle policy.
pub trait HookJournalPort: Send + Sync {
    fn reached(
        &self,
        producer: &Producer,
        occurrence: &HookOccurrence,
        plan: &HookPlan,
    ) -> Result<HookReachOutcome, HookReachError>;
    fn started(
        &self,
        spec: &HookSpec,
        occurrence: &HookOccurrence,
        reached: &HookReachedEvidence,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> Result<HookStartedEvidence, HookStartError>;
    fn settled(
        &self,
        occurrence: &HookOccurrence,
        started: HookStartedEvidence,
        result: &HookResult,
    ) -> Result<EventId, HookRunError>;
}

/// The sole external invocation effect consumed by the lifecycle policy.
pub trait HookInvocationPort: Send + Sync {
    fn invoke<'a>(&'a self, request: HookInvocationRequest) -> HookFuture<'a>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunError(String);

impl HookRunError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}
impl fmt::Display for HookRunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for HookRunError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookReachError {
    Durable {
        evidence: HookReachedEvidence,
        failure: HookRunError,
    },
    Absent(HookRunError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookStartError {
    Durable {
        evidence: HookStartedEvidence,
        failure: HookRunError,
    },
    Absent(HookRunError),
}

macro_rules! durable_error {
    ($name:ty) => {
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    Self::Durable { failure, .. } | Self::Absent(failure) => failure.fmt(formatter),
                }
            }
        }
        impl std::error::Error for $name {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                match self {
                    Self::Durable { failure, .. } | Self::Absent(failure) => Some(failure),
                }
            }
        }
    };
}
durable_error!(HookReachError);
durable_error!(HookStartError);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunResult {
    decision: HookDecision,
    feedback: Vec<HookFeedback>,
    observer_failure: bool,
    durable: bool,
    skipped_once: bool,
    started_invocation: bool,
    blocking_occurrence: Option<EventId>,
    blocking_invocation: Option<HookInvocationId>,
    closure_event: Option<EventId>,
}

impl HookRunResult {
    pub fn decision(&self) -> HookDecision {
        self.decision
    }
    pub fn feedback(&self) -> &[HookFeedback] {
        &self.feedback
    }
    pub fn feedback_bytes(&self) -> usize {
        self.feedback.iter().fold(0usize, |bytes, feedback| {
            bytes
                .saturating_add(feedback.head_bytes())
                .saturating_add(feedback.tail_bytes())
        })
    }
    pub fn is_blocked(&self) -> bool {
        self.decision.is_blocking()
    }
    pub fn is_observer_failure(&self) -> bool {
        self.observer_failure
    }
    pub fn is_durable(&self) -> bool {
        self.durable
    }
    pub fn reverted_effect(&self) -> bool {
        false
    }
    pub fn was_skipped_once(&self) -> bool {
        self.skipped_once
    }
    pub fn started_invocation(&self) -> bool {
        self.started_invocation
    }
    pub fn blocking_occurrence(&self) -> Option<EventId> {
        self.blocking_occurrence
    }
    pub fn blocking_invocation(&self) -> Option<HookInvocationId> {
        self.blocking_invocation
    }
    pub fn closure_event(&self) -> Option<EventId> {
        self.closure_event
    }
}

#[derive(Clone)]
pub struct HookRunner {
    once: Arc<Mutex<OnceAdmission>>,
}

type OnceKey = (SkillInvocationId, HookId);

#[derive(Default)]
struct OnceAdmission {
    consumed: HashSet<OnceKey>,
    pending: HashMap<OnceKey, Vec<oneshot::Sender<()>>>,
}

struct OnceReservations {
    once: Arc<Mutex<OnceAdmission>>,
    keys: Vec<OnceKey>,
}

impl OnceReservations {
    fn new(once: Arc<Mutex<OnceAdmission>>, keys: Vec<OnceKey>) -> Self {
        Self { once, keys }
    }

    fn consume(&mut self, key: OnceKey) {
        let mut once = self.once.lock().expect("once hook admission");
        once.consumed.insert(key);
        if let Some(waiters) = once.pending.remove(&key) {
            wake_once_waiters(waiters);
        }
        self.keys.retain(|reserved| *reserved != key);
    }

    fn release(&mut self) {
        let mut once = self.once.lock().expect("once hook admission");
        for key in self.keys.drain(..) {
            if let Some(waiters) = once.pending.remove(&key) {
                wake_once_waiters(waiters);
            }
        }
    }
}

impl Drop for OnceReservations {
    fn drop(&mut self) {
        // Cancellation before a durable start cannot leave a stale once bit.
        self.release();
    }
}

impl HookRunner {
    pub fn new() -> Self {
        Self::new_with_durable_started(std::iter::empty())
    }

    pub fn new_with_durable_started<I>(started: I) -> Self
    where
        I: IntoIterator<Item = HookStartedEvidence>,
    {
        let consumed = started
            .into_iter()
            .filter_map(|record| match record.activation {
                HookActivation::Skill { invocation } => Some((invocation, record.hook)),
                HookActivation::Persistent => None,
            })
            .collect();
        Self {
            once: Arc::new(Mutex::new(OnceAdmission {
                consumed,
                pending: HashMap::new(),
            })),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn run_occurrence(
        &self,
        journal: &dyn HookJournalPort,
        invoker: &dyn HookInvocationPort,
        parent_cancel: CancelToken,
        parent_deadline: Instant,
        catalogue: &HookCatalogue,
        producer: Producer,
        occurrence: HookOccurrence,
    ) -> Result<HookRunResult, HookRunError> {
        let phase = occurrence.event.lifecycle.phase();
        let mut result = empty_result();
        if !producer.may_trigger_hooks() {
            return Ok(result);
        }

        let (specs, mut reservations, skipped_once) =
            self.reserve_plan(catalogue, &occurrence.event).await;
        result.skipped_once = skipped_once;
        let plan = match HookPlan::new(specs.iter().map(|spec| spec.id()).collect()) {
            Ok(plan) => plan,
            Err(error) => {
                reservations.release();
                return Err(HookRunError::new(error.to_string()));
            }
        };
        let reached = match journal.reached(&producer, &occurrence, &plan) {
            Ok(HookReachOutcome::Fresh(evidence)) => {
                if let Err(error) =
                    validate_reached_evidence(&producer, &occurrence, &plan, &evidence)
                {
                    reservations.release();
                    return Err(error);
                }
                evidence
            }
            Ok(HookReachOutcome::Recovered {
                evidence,
                starts,
                settlements,
            }) => {
                if let Err(error) = validate_reached_identity(&producer, &occurrence, &evidence)
                    .and_then(|()| validate_recovered_prefix(&evidence, &starts, &settlements))
                {
                    reservations.release();
                    return Err(error);
                }
                self.finish_recovered(&mut reservations, &starts);
                apply_recovered_settlements(&mut result, phase, &evidence, &starts, &settlements);
                if starts.len() != evidence.plan().len()
                    || settlements.len() != evidence.plan().len()
                {
                    record_occurrence_uncertainty(&mut result, phase, &evidence, starts.last());
                }
                return Ok(result);
            }
            Err(HookReachError::Durable { evidence, .. }) => {
                if let Err(error) =
                    validate_reached_evidence(&producer, &occurrence, &plan, &evidence)
                {
                    reservations.release();
                    return Err(error);
                }
                reservations.release();
                record_occurrence_uncertainty(&mut result, phase, &evidence, None);
                return Ok(result);
            }
            Err(HookReachError::Absent(_)) => {
                reservations.release();
                record_absent_failure(&mut result, phase);
                return Ok(result);
            }
        };
        result.durable = true;
        result.closure_event = Some(reached.event());

        for spec in specs {
            let invocation = HookInvocationId::new();
            let activation = spec
                .skill_invocation()
                .map_or(HookActivation::Persistent, |invocation| {
                    HookActivation::Skill { invocation }
                });
            let started = match journal.started(spec, &occurrence, &reached, activation, invocation)
            {
                Ok(evidence) => evidence,
                Err(HookStartError::Durable { evidence, .. }) => {
                    if let Err(error) =
                        validate_start_evidence(spec, &reached, invocation, activation, evidence)
                    {
                        reservations.release();
                        return Err(error);
                    }
                    self.finish_start(&mut reservations, &evidence);
                    reservations.release();
                    record_occurrence_uncertainty(&mut result, phase, &reached, Some(&evidence));
                    return Ok(result);
                }
                Err(HookStartError::Absent(_)) => {
                    reservations.release();
                    record_occurrence_uncertainty(&mut result, phase, &reached, None);
                    return Ok(result);
                }
            };
            if let Err(error) =
                validate_start_evidence(spec, &reached, invocation, activation, started)
            {
                reservations.release();
                return Err(error);
            }
            self.finish_start(&mut reservations, &started);
            result.started_invocation = true;

            let request = HookInvocationRequest::new(
                started,
                occurrence.causality.clone(),
                spec.handler().clone(),
                occurrence.event.clone(),
                EffectScope::with_parent(
                    parent_cancel.clone(),
                    Some(parent_deadline),
                    spec.handler().timeout_for(occurrence.event.lifecycle),
                ),
            );
            let invoked = invoker.invoke(request).await;
            let settled = normalize_result(phase, occurrence.event.lifecycle, invoked);
            let settlement = match journal.settled(&occurrence, started, &settled) {
                Ok(event) => event,
                Err(_) => {
                    reservations.release();
                    record_occurrence_uncertainty(&mut result, phase, &reached, Some(&started));
                    return Ok(result);
                }
            };
            result.closure_event = Some(settlement);
            apply_result(
                &mut result,
                phase,
                &reached,
                Some(started.invocation()),
                &settled,
            );
        }
        Ok(result)
    }

    async fn reserve_plan<'a>(
        &self,
        catalogue: &'a HookCatalogue,
        event: &HookEvent,
    ) -> (Vec<&'a HookSpec>, OnceReservations, bool) {
        loop {
            let matching = catalogue.matching(event);
            let waiting = {
                let mut once = self.once.lock().expect("once hook admission");
                let mut waiting = Vec::new();
                for spec in &matching {
                    let Some(key) = once_key(spec) else {
                        continue;
                    };
                    if let Some(waiters) = once.pending.get_mut(&key) {
                        let (sender, receiver) = oneshot::channel();
                        waiters.push(sender);
                        waiting.push(receiver);
                    }
                }
                if !waiting.is_empty() {
                    waiting
                } else {
                    let mut skipped = false;
                    let mut reservations = Vec::new();
                    let specs = matching
                        .into_iter()
                        .filter(|spec| {
                            let Some(key) = once_key(spec) else {
                                return true;
                            };
                            if once.consumed.contains(&key) {
                                skipped = true;
                                return false;
                            }
                            once.pending.insert(key, Vec::new());
                            reservations.push(key);
                            true
                        })
                        .collect();
                    // Reserve the complete plan before its durable reach so a
                    // concurrent occurrence cannot admit a later once spec.
                    return (
                        specs,
                        OnceReservations::new(Arc::clone(&self.once), reservations),
                        skipped,
                    );
                }
            };

            let _ = join_all(waiting).await;
        }
    }

    fn finish_start(&self, reservations: &mut OnceReservations, evidence: &HookStartedEvidence) {
        let key = match evidence.activation() {
            HookActivation::Skill { invocation } => (invocation, evidence.hook()),
            HookActivation::Persistent => return,
        };
        reservations.consume(key);
    }

    fn finish_recovered(
        &self,
        reservations: &mut OnceReservations,
        starts: &[HookStartedEvidence],
    ) {
        for started in starts {
            self.finish_start(reservations, started);
        }
        reservations.release();
    }
}

impl Default for HookRunner {
    fn default() -> Self {
        Self::new()
    }
}

fn once_key(spec: &HookSpec) -> Option<OnceKey> {
    if spec.once() {
        spec.skill_invocation()
            .map(|invocation| (invocation, spec.id()))
    } else {
        None
    }
}

fn wake_once_waiters(waiters: Vec<oneshot::Sender<()>>) {
    for waiter in waiters {
        let _ = waiter.send(());
    }
}

#[derive(Clone)]
pub struct HookLifecycleDispatcher {
    runner: HookRunner,
    catalogue: Arc<HookCatalogue>,
}

impl HookLifecycleDispatcher {
    pub fn new(runner: HookRunner, catalogue: HookCatalogue) -> Self {
        Self {
            runner,
            catalogue: Arc::new(catalogue),
        }
    }

    pub async fn dispatch(
        &self,
        journal: &dyn HookJournalPort,
        invoker: &dyn HookInvocationPort,
        parent_cancel: CancelToken,
        parent_deadline: Instant,
        producer: Producer,
        occurrence: HookOccurrence,
    ) -> Result<HookRunResult, HookRunError> {
        self.runner
            .run_occurrence(
                journal,
                invoker,
                parent_cancel,
                parent_deadline,
                &self.catalogue,
                producer,
                occurrence,
            )
            .await
    }
}

/// Borrow-scoped authority that binds one session's hook policy and effects.
#[derive(Clone)]
pub struct SessionHookRuntime<'s> {
    dispatcher: HookLifecycleDispatcher,
    journal: SharedJournal<'s>,
    invoker: Arc<dyn HookInvocationPort + 's>,
    parent_cancel: CancelToken,
    parent_deadline: Instant,
}

impl<'s> SessionHookRuntime<'s> {
    pub fn new(
        dispatcher: HookLifecycleDispatcher,
        journal: SharedJournal<'s>,
        invoker: Arc<dyn HookInvocationPort + 's>,
        parent_cancel: CancelToken,
        parent_deadline: Instant,
    ) -> Self {
        Self {
            dispatcher,
            journal,
            invoker,
            parent_cancel,
            parent_deadline,
        }
    }

    pub async fn dispatch(
        &self,
        producer: Producer,
        occurrence: HookOccurrence,
    ) -> Result<HookRunResult, HookRunError> {
        self.dispatcher
            .dispatch(
                &self.journal,
                self.invoker.as_ref(),
                self.parent_cancel.clone(),
                self.parent_deadline,
                producer,
                occurrence,
            )
            .await
    }

    pub fn journal(&self) -> SharedJournal<'s> {
        self.journal.clone()
    }

    pub fn inspect<T>(
        &self,
        inspect: impl FnOnce(&SessionState) -> T,
    ) -> Result<T, SharedJournalError> {
        self.journal.inspect(inspect)
    }

    pub fn state(&self) -> SessionState {
        self.inspect(Clone::clone)
            .expect("session hook runtime journal inspection")
    }

    pub fn try_into_state(self) -> Result<SessionState, SharedJournalError> {
        self.journal.try_into_state()
    }
}

fn validate_reached_evidence(
    producer: &Producer,
    occurrence: &HookOccurrence,
    plan: &HookPlan,
    evidence: &HookReachedEvidence,
) -> Result<(), HookRunError> {
    validate_reached_identity(producer, occurrence, evidence)?;
    if evidence.plan() != plan {
        return Err(HookRunError::new(
            "durable hook reach evidence does not match its owner occurrence",
        ));
    }
    Ok(())
}

fn validate_reached_identity(
    producer: &Producer,
    occurrence: &HookOccurrence,
    evidence: &HookReachedEvidence,
) -> Result<(), HookRunError> {
    if evidence.occurrence() != occurrence.key()
        || evidence.subject() != occurrence.event.subject()
        || evidence.producer() != producer
    {
        return Err(HookRunError::new(
            "durable hook reach evidence does not match its owner occurrence",
        ));
    }
    Ok(())
}

fn validate_start_evidence(
    spec: &HookSpec,
    reached: &HookReachedEvidence,
    invocation: HookInvocationId,
    activation: HookActivation,
    evidence: HookStartedEvidence,
) -> Result<(), HookRunError> {
    if evidence.occurrence() != reached.event()
        || evidence.hook() != spec.id()
        || evidence.invocation() != invocation
        || evidence.activation() != activation
    {
        return Err(HookRunError::new(
            "durable hook start evidence does not match its occurrence",
        ));
    }
    Ok(())
}

fn normalize_result(
    phase: HookPhase,
    lifecycle: HookLifecycle,
    result: Result<HookOutcome, HookFailure>,
) -> HookResult {
    match result {
        Ok(outcome)
            if (phase.is_observer() && outcome.decision().is_blocking())
                || (outcome.decision().is_escalation()
                    && lifecycle != HookLifecycle::PreToolUse) =>
        {
            HookResult::Failure {
                failure: HookFailure::new(
                    crate::event::hook::HookFailureKind::MalformedResult,
                    outcome.feedback().cloned(),
                ),
            }
        }
        Ok(outcome) => HookResult::Outcome { outcome },
        Err(failure) => HookResult::Failure { failure },
    }
}

fn validate_recovered_prefix(
    reached: &HookReachedEvidence,
    starts: &[HookStartedEvidence],
    settlements: &[HookSettledEvidence],
) -> Result<(), HookRunError> {
    if starts.len() > reached.plan().len() || settlements.len() > starts.len() {
        return Err(HookRunError::new(
            "recovered hook occurrence exceeds its frozen plan",
        ));
    }
    for (planned, started) in reached.plan().as_slice().iter().zip(starts) {
        if *planned != started.hook() || started.occurrence() != reached.event() {
            return Err(HookRunError::new(
                "recovered hook start does not match its frozen plan",
            ));
        }
    }
    for (started, settled) in starts.iter().zip(settlements) {
        if settled.occurrence() != reached.event()
            || settled.hook() != started.hook()
            || settled.invocation() != started.invocation()
        {
            return Err(HookRunError::new(
                "recovered hook settlement does not match its start",
            ));
        }
    }
    Ok(())
}

fn apply_recovered_settlements(
    aggregate: &mut HookRunResult,
    phase: HookPhase,
    reached: &HookReachedEvidence,
    starts: &[HookStartedEvidence],
    settlements: &[HookSettledEvidence],
) {
    for (started, settled) in starts.iter().zip(settlements) {
        apply_result(
            aggregate,
            phase,
            reached,
            Some(started.invocation()),
            settled.result(),
        );
    }
    aggregate.durable = true;
    aggregate.closure_event = if settlements.len() == reached.plan().len() {
        settlements
            .last()
            .map(HookSettledEvidence::event)
            .or(Some(reached.event()))
    } else {
        None
    };
}

fn apply_result(
    aggregate: &mut HookRunResult,
    phase: HookPhase,
    reached: &HookReachedEvidence,
    invocation: Option<HookInvocationId>,
    result: &HookResult,
) {
    if let Some(feedback) = result.feedback() {
        let next = aggregate
            .feedback_bytes()
            .saturating_add(feedback.head_bytes())
            .saturating_add(feedback.tail_bytes());
        if next <= MAX_FEEDBACK_BYTES {
            aggregate.feedback.push(feedback.clone());
        }
    }
    match result {
        HookResult::Outcome { outcome } if outcome.decision().is_blocking() => {
            aggregate.decision = HookDecision::Block;
            aggregate.blocking_occurrence.get_or_insert(reached.event());
            if let Some(invocation) = invocation {
                aggregate.blocking_invocation.get_or_insert(invocation);
            }
        }
        HookResult::Outcome { outcome }
            if outcome.decision().is_escalation() && !aggregate.decision.is_blocking() =>
        {
            aggregate.decision = HookDecision::RequestEscalation;
        }
        HookResult::Failure { .. } if phase.is_pre_effect() => {
            aggregate.decision = HookDecision::Block;
            aggregate.blocking_occurrence.get_or_insert(reached.event());
            if let Some(invocation) = invocation {
                aggregate.blocking_invocation.get_or_insert(invocation);
            }
        }
        HookResult::Failure { .. } => aggregate.observer_failure = true,
        HookResult::Outcome { .. } => {}
    }
    aggregate.durable = true;
}

fn record_occurrence_uncertainty(
    result: &mut HookRunResult,
    phase: HookPhase,
    evidence: &HookReachedEvidence,
    started: Option<&HookStartedEvidence>,
) {
    if phase.is_pre_effect() {
        result.decision = HookDecision::Block;
        result.blocking_occurrence.get_or_insert(evidence.event());
        if let Some(started) = started {
            result
                .blocking_invocation
                .get_or_insert(started.invocation());
        }
    } else {
        result.observer_failure = true;
    }
    result.durable = true;
}

fn record_absent_failure(result: &mut HookRunResult, phase: HookPhase) {
    if phase.is_pre_effect() {
        result.decision = HookDecision::Block;
    } else {
        result.observer_failure = true;
    }
}

fn empty_result() -> HookRunResult {
    HookRunResult {
        decision: HookDecision::Continue,
        feedback: Vec::new(),
        observer_failure: false,
        durable: false,
        skipped_once: false,
        started_invocation: false,
        blocking_occurrence: None,
        blocking_invocation: None,
        closure_event: None,
    }
}
