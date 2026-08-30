//! Typed hook catalogue admission and durable invocation policy.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::lock::Mutex as AsyncMutex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::{ConfigLayer, Source, Sourced, Trust};
use crate::digest::sha256_hex;
use crate::event::domain::EventKind;
use crate::event::hook::{
    HookActivation, HookContentDigest, HookDecision, HookDescriptorEvidence, HookFailure, HookId,
    HookInvocationId, HookLifecycle, HookOutcome, HookPhase, HookSourceAuthority,
    HookSourceLocation, HookSubject,
};
use crate::event::skill::{SkillInvocationId, SkillSourceAuthority};
use crate::permit::rule::{ArgvToken, Rule, Segment};
use crate::turn::CancelToken;

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
        HookSortKey(authority, self.evidence.ordinal(), self.id().to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct HookSortKey(u8, u32, String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookEvent {
    lifecycle: HookLifecycle,
    subject: HookSubject,
    selector: Option<String>,
    permission_selector: Option<Rule>,
    hook_originated: bool,
}

impl HookEvent {
    pub fn session(lifecycle: HookLifecycle) -> Self {
        Self {
            lifecycle,
            subject: HookSubject::Session,
            selector: None,
            permission_selector: None,
            hook_originated: false,
        }
    }

    pub fn turn(lifecycle: HookLifecycle, subject: HookSubject) -> Self {
        Self {
            lifecycle,
            subject,
            selector: None,
            permission_selector: None,
            hook_originated: false,
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
            hook_originated: false,
        }
    }

    pub fn lifecycle(&self) -> HookLifecycle {
        self.lifecycle
    }

    pub fn subject(&self) -> HookSubject {
        self.subject
    }

    pub fn originated_by_hook(mut self) -> Self {
        self.hook_originated = true;
        self
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
    handler: HookHandlerConfig,
    event: HookEvent,
    activation: HookActivation,
    invocation: HookInvocationId,
    effect_scope: EffectScope,
}

impl HookInvocationRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        handler: HookHandlerConfig,
        lifecycle: HookLifecycle,
        matcher: Option<HookMatcher>,
        subject: HookSubject,
        activation: HookActivation,
        invocation: HookInvocationId,
        effect_scope: EffectScope,
    ) -> Self {
        let selector = matcher.as_ref().map(|matcher| match matcher {
            HookMatcher::Permission(rule) => rule.to_string(),
            HookMatcher::Exact(value) => value.clone(),
        });
        let permission_selector = matcher.and_then(|matcher| match matcher {
            HookMatcher::Permission(rule) => Some(rule),
            HookMatcher::Exact(_) => None,
        });
        Self {
            handler,
            event: HookEvent {
                lifecycle,
                subject,
                selector,
                permission_selector,
                hook_originated: true,
            },
            activation,
            invocation,
            effect_scope,
        }
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
        self.activation
    }

    pub fn invocation(&self) -> HookInvocationId {
        self.invocation
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
            invocation_id: request.invocation,
            activation: request.activation,
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
        for spec in &specs {
            if !ids.insert(spec.id()) {
                return Err(HookCatalogueError::DuplicateIdentity {
                    exact_name: spec.id().to_string(),
                });
            }
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
        self.specs
            .iter()
            .filter(|spec| spec.matches(event))
            .collect()
    }
    pub fn serialized_config(&self) -> String {
        self.specs
            .iter()
            .map(|spec| spec.id().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Durable evidence sufficient to reconstruct one consumed skill-once key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HookStartedEvidence {
    hook: HookId,
    invocation: HookInvocationId,
    activation: HookActivation,
}

impl HookStartedEvidence {
    pub fn new(hook: HookId, invocation: HookInvocationId, activation: HookActivation) -> Self {
        Self {
            hook,
            invocation,
            activation,
        }
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

    pub fn from_event(event: &EventKind) -> Option<Self> {
        match event {
            EventKind::HookInvocationStarted {
                hook,
                invocation,
                activation,
                ..
            } => Some(Self::new(*hook, *invocation, *activation)),
            _ => None,
        }
    }
}

/// Read-only replay input accepted from a durable journal projection.
pub trait DurableHookStarts {
    fn snapshot(self) -> Vec<HookStartedEvidence>;
}

impl DurableHookStarts for Vec<HookStartedEvidence> {
    fn snapshot(self) -> Vec<HookStartedEvidence> {
        self
    }
}

impl DurableHookStarts for Arc<Mutex<Vec<HookStartedEvidence>>> {
    fn snapshot(self) -> Vec<HookStartedEvidence> {
        self.lock()
            .expect("hook start evidence mutex poisoned")
            .clone()
    }
}

pub type HookFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HookOutcome, HookFailure>> + Send + 'a>>;
pub type HookStartFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type HookRecordFuture<'a> = Pin<Box<dyn Future<Output = Result<(), HookRunError>> + Send + 'a>>;

pub trait HookPort: Send + Sync {
    fn started<'a>(
        &'a self,
        spec: &'a HookSpec,
        event: &'a HookEvent,
        activation: HookActivation,
        invocation: HookInvocationId,
    ) -> HookStartFuture<'a, Result<HookStartedEvidence, HookStartError>>;
    fn invoke<'a>(&'a self, spec: &'a HookSpec, event: HookEvent) -> HookFuture<'a>;
    fn settled<'a>(
        &'a self,
        spec: &'a HookSpec,
        invocation: HookInvocationId,
        outcome: Result<&'a HookOutcome, &'a HookFailure>,
    ) -> HookRecordFuture<'a>;
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
pub enum HookStartError {
    Durable {
        evidence: HookStartedEvidence,
        failure: HookRunError,
    },
    Absent(HookRunError),
}

impl fmt::Display for HookStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Durable { failure, .. } | Self::Absent(failure) => failure.fmt(formatter),
        }
    }
}

impl std::error::Error for HookStartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Durable { failure, .. } | Self::Absent(failure) => Some(failure),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunResult {
    decision: HookDecision,
    feedback_bytes: usize,
    observer_failure: bool,
    durable: bool,
    skipped_once: bool,
    started_invocation: bool,
    blocking_invocation: Option<HookInvocationId>,
}

impl HookRunResult {
    pub fn decision(&self) -> HookDecision {
        self.decision
    }
    pub fn feedback_bytes(&self) -> usize {
        self.feedback_bytes
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

    pub fn blocking_invocation(&self) -> Option<HookInvocationId> {
        self.blocking_invocation
    }
}

pub struct HookRunner<P> {
    port: P,
    consumed_once: AsyncMutex<HashSet<(SkillInvocationId, HookId)>>,
}

impl<P: HookPort> HookRunner<P> {
    pub fn new(port: P) -> Self {
        Self {
            port,
            consumed_once: AsyncMutex::new(HashSet::new()),
        }
    }

    pub fn new_with_durable_started(port: P, started: impl DurableHookStarts) -> Self {
        let consumed_once = started
            .snapshot()
            .into_iter()
            .filter_map(|record| match record.activation {
                HookActivation::Skill { invocation } => Some((invocation, record.hook)),
                HookActivation::Persistent => None,
            })
            .collect();
        Self {
            port,
            consumed_once: AsyncMutex::new(consumed_once),
        }
    }

    pub async fn run(
        &self,
        catalogue: &HookCatalogue,
        event: HookEvent,
    ) -> Result<HookRunResult, HookRunError> {
        let phase = event.lifecycle.phase();
        let mut result = empty_result();
        if Self::hook_originated_event(&event) {
            return Ok(result);
        }
        for spec in catalogue.matching(&event) {
            let invocation = HookInvocationId::new();
            let activation = spec
                .skill_invocation()
                .map_or(HookActivation::Persistent, |invocation| {
                    HookActivation::Skill { invocation }
                });
            let once_key = if spec.once() {
                Some((
                    spec.skill_invocation()
                        .ok_or_else(|| HookRunError::new("persistent once hook reached runtime"))?,
                    spec.id(),
                ))
            } else {
                None
            };
            let mut consumed = if once_key.is_some() {
                Some(self.consumed_once.lock().await)
            } else {
                None
            };
            if let (Some(key), Some(consumed)) = (once_key, consumed.as_ref())
                && consumed.contains(&key)
            {
                result.skipped_once = true;
                continue;
            }

            let start = self
                .port
                .started(spec, &event, activation, invocation)
                .await;
            match start {
                Ok(evidence) => {
                    validate_start_evidence(spec, invocation, activation, evidence)?;
                    if let Some(consumed) = consumed.as_mut() {
                        consume_once_evidence(consumed, evidence)?;
                    }
                }
                Err(HookStartError::Durable { evidence, .. }) => {
                    validate_start_evidence(spec, invocation, activation, evidence)?;
                    if let Some(consumed) = consumed.as_mut() {
                        consume_once_evidence(consumed, evidence)?;
                    }
                    record_start_failure(&mut result, phase, Some(evidence));
                    continue;
                }
                Err(HookStartError::Absent(_)) => {
                    record_start_failure(&mut result, phase, None);
                    continue;
                }
            }
            drop(consumed);
            result.started_invocation = true;
            let settlement = self.port.invoke(spec, event.clone()).await;
            let durable = self
                .port
                .settled(spec, invocation, settlement.as_ref())
                .await
                .is_ok();
            result.durable |= durable;
            match settlement {
                Ok(outcome) => {
                    result.feedback_bytes = result
                        .feedback_bytes
                        .saturating_add(outcome.feedback().map_or(0, |feedback| {
                            feedback.head_bytes().saturating_add(feedback.tail_bytes())
                        }))
                        .min(MAX_FEEDBACK_BYTES);
                    if outcome.decision().is_blocking() {
                        result.decision = HookDecision::Block;
                        result.blocking_invocation.get_or_insert(invocation);
                    } else if outcome.decision().is_escalation() && !result.decision.is_blocking() {
                        result.decision = HookDecision::RequestEscalation;
                    }
                }
                Err(_) if phase == HookPhase::PreEffect => {
                    result.decision = HookDecision::Block;
                    result.blocking_invocation.get_or_insert(invocation);
                }
                Err(_) => {
                    result.observer_failure = true;
                    result.durable |= durable;
                }
            }
        }
        Ok(result)
    }

    pub fn hook_originated_event(event: &HookEvent) -> bool {
        event.hook_originated
    }
}

fn validate_start_evidence(
    spec: &HookSpec,
    invocation: HookInvocationId,
    activation: HookActivation,
    evidence: HookStartedEvidence,
) -> Result<(), HookRunError> {
    if evidence.hook() != spec.id()
        || evidence.invocation() != invocation
        || evidence.activation() != activation
    {
        return Err(HookRunError::new(
            "durable hook start evidence does not match its invocation",
        ));
    }
    Ok(())
}

fn consume_once_evidence(
    consumed: &mut HashSet<(SkillInvocationId, HookId)>,
    evidence: HookStartedEvidence,
) -> Result<(), HookRunError> {
    let HookActivation::Skill { invocation } = evidence.activation() else {
        return Err(HookRunError::new(
            "once hook durable start lost its skill activation",
        ));
    };
    consumed.insert((invocation, evidence.hook()));
    Ok(())
}

fn record_start_failure(
    result: &mut HookRunResult,
    phase: HookPhase,
    evidence: Option<HookStartedEvidence>,
) {
    if phase == HookPhase::PreEffect {
        result.decision = HookDecision::Block;
        if let Some(evidence) = evidence {
            result
                .blocking_invocation
                .get_or_insert(evidence.invocation());
        }
    } else {
        result.observer_failure = true;
    }
    result.durable |= evidence.is_some();
}

fn empty_result() -> HookRunResult {
    HookRunResult {
        decision: HookDecision::Continue,
        feedback_bytes: 0,
        observer_failure: false,
        durable: false,
        skipped_once: false,
        started_invocation: false,
        blocking_invocation: None,
    }
}
