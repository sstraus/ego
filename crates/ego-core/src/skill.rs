//! Admitted skills, their one typed operation, and invocation-scoped authority.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::config::{ModelRef, Trust};
use crate::event::domain::EventKind;
use crate::event::hook::{
    HookActivation, HookCatalogueEntryEvidence, HookDescriptorEvidence, HookLifecycle,
};
use crate::event::id::{EventId, ItemId, SubagentInvocationId, TurnId};
use crate::event::skill::{
    SkillCatalogueEvidence, SkillCatalogueGeneration, SkillDescriptorEvidence, SkillId,
    SkillInvocationId, SkillSourceAuthority, SkillSourceLocation,
};
use crate::hook::{HookCatalogue, HookConfigEntry, HookHandlerConfig, HookMatcher, HookSource};
use crate::permit::rule::PathPattern;
use crate::permit::{CanonPath, GuardedIo, PathResolver};
use crate::store::blob::{BlobRef, BlobStore};
use crate::subagent::{ChildWait, ChildWaitResult};
use crate::turn::SharedJournal;

const MAX_ADMISSION_BYTES: usize = 64 * 1024;

/// A closed provider and host transport. Only these two operations cross the
/// skill boundary; admitted invocation metadata is deliberately not part of
/// the wire representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SkillOperation {
    Invoke {
        name: String,
        arguments: String,
        /// The caller spending this child without its own posture.
        ///
        /// The role's own `bare` declaration answers "this role is never
        /// trusted with a workspace"; this answers "not this time". An
        /// orchestrator that knows what it is about to hand a subagent is the
        /// one deciding its level of engagement, and a repository it has not
        /// vetted is exactly the case the declaration cannot anticipate.
        /// Absent means inherit, and neither source can raise a child above
        /// the parent that read it.
        #[serde(default)]
        bare: bool,
    },
    ReadResource {
        invocation: SkillInvocationId,
        path: String,
    },
}

impl SkillOperation {
    pub fn name(&self) -> &str {
        match self {
            Self::Invoke { name, .. } => name,
            Self::ReadResource { .. } => "skill",
        }
    }

    pub fn raw_arguments(&self) -> &str {
        match self {
            Self::Invoke { arguments, .. } => arguments,
            Self::ReadResource { .. } => "",
        }
    }

    /// Whether the caller asked for this one child to be spent bare.
    pub const fn bare(&self) -> bool {
        matches!(self, Self::Invoke { bare: true, .. })
    }
}

/// User-surface invocation bound to the descriptor selected from one action
/// snapshot. This never crosses the provider wire boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedSkillOperation {
    operation: SkillOperation,
    descriptor: SkillId,
}

impl AdmittedSkillOperation {
    pub fn invoke(descriptor: &SkillDescriptorEvidence, arguments: impl Into<String>) -> Self {
        Self {
            operation: SkillOperation::Invoke {
                name: descriptor.name.clone(),
                arguments: arguments.into(),
                bare: false,
            },
            descriptor: descriptor.id,
        }
    }

    pub fn name(&self) -> &str {
        self.operation.name()
    }

    pub fn raw_arguments(&self) -> &str {
        self.operation.raw_arguments()
    }

    pub fn descriptor_id(&self) -> SkillId {
        self.descriptor
    }

    pub fn into_operation(self) -> SkillOperation {
        self.operation
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillError(String);

impl SkillError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for SkillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for SkillError {}

/// Host roots corresponding to the three admitted source authorities.
#[derive(Debug, Clone)]
pub struct SkillResourceRoots {
    system: PathBuf,
    user: PathBuf,
    project: PathBuf,
}

impl SkillResourceRoots {
    pub fn new(system: PathBuf, user: PathBuf, project: PathBuf) -> Self {
        Self {
            system,
            user,
            project,
        }
    }

    pub(crate) fn root(&self, authority: SkillSourceAuthority) -> &Path {
        match authority {
            SkillSourceAuthority::System => &self.system,
            SkillSourceAuthority::User => &self.user,
            SkillSourceAuthority::Project => &self.project,
        }
    }
}

/// One fully admitted skill. It contains typed policy, never YAML nodes or an
/// absolute host path.
#[derive(Clone)]
pub struct SkillSpec {
    descriptor: SkillDescriptorEvidence,
    body: Arc<[u8]>,
    model: Option<ModelRef>,
    effort: Option<String>,
    allowed_tools: Option<BTreeSet<String>>,
    disallowed_tools: BTreeSet<String>,
    fork: Option<ChildWait>,
    profile: Option<String>,
    /// Whether this role is spawned without its parent's workspace posture.
    ///
    /// A forked skill runs at the posture its parent was admitted at: it is
    /// subject to the same project rules, hooks included. `bare` is how a
    /// role says otherwise — a reviewer that should read a repository without
    /// running anything the repository declares. It can only take authority
    /// away: a bare child of a trusted parent is untrusted, and no value here
    /// makes a child of an untrusted parent trusted.
    bare: bool,
    hooks: Vec<HookConfigEntry>,
    resource_paths: Vec<String>,
    arguments: Vec<String>,
    metadata: BTreeMap<String, String>,
}

impl fmt::Debug for SkillSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SkillSpec")
            .field("descriptor", &self.descriptor)
            .field("body_bytes", &self.body.len())
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("fork", &self.fork)
            .field("profile", &self.profile)
            .finish()
    }
}

impl SkillSpec {
    pub fn new(descriptor: SkillDescriptorEvidence, body: Vec<u8>) -> Result<Self, SkillError> {
        if body.is_empty() {
            return Err(SkillError::new("skill body must not be empty"));
        }
        std::str::from_utf8(&body)
            .map_err(|_| SkillError::new("skill body must be valid UTF-8"))?;
        Ok(Self {
            descriptor,
            body: body.into(),
            model: None,
            effort: None,
            allowed_tools: None,
            disallowed_tools: BTreeSet::new(),
            fork: None,
            profile: None,
            bare: false,
            hooks: Vec::new(),
            resource_paths: Vec::new(),
            arguments: Vec::new(),
            metadata: BTreeMap::new(),
        })
    }

    pub fn descriptor(&self) -> &SkillDescriptorEvidence {
        &self.descriptor
    }

    pub fn name(&self) -> &str {
        &self.descriptor.name
    }

    pub fn source(&self) -> SkillSourceAuthority {
        self.descriptor.source
    }

    pub fn agent(&self) -> Option<&str> {
        self.profile()
    }

    pub fn with_model(mut self, model: ModelRef, effort: impl Into<String>) -> Self {
        self.model = Some(model);
        self.effort = Some(effort.into());
        self
    }

    pub fn with_model_selection(mut self, model: Option<ModelRef>, effort: Option<String>) -> Self {
        self.model = model;
        self.effort = effort;
        self
    }

    pub fn with_tool_ceiling<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_tools = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn with_disallowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.disallowed_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_fork(mut self, wait: ChildWait) -> Self {
        self.fork = Some(wait);
        self
    }

    pub fn with_profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = Some(profile.into());
        self
    }

    #[must_use]
    pub const fn with_bare(mut self, bare: bool) -> Self {
        self.bare = bare;
        self
    }

    pub fn with_hook_entries(mut self, hooks: Vec<HookConfigEntry>) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_arguments<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_metadata(mut self, metadata: BTreeMap<String, String>) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_resource_paths<I, S>(mut self, paths: I) -> Result<Self, SkillError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let paths: Vec<String> = paths.into_iter().map(Into::into).collect();
        for path in &paths {
            PathPattern::parse(path).map_err(|error| SkillError::new(error.to_string()))?;
        }
        self.resource_paths = paths;
        Ok(self)
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_ref().map(|model| model.0.as_str())
    }

    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }

    pub fn allowed_tools(&self) -> Option<&BTreeSet<String>> {
        self.allowed_tools.as_ref()
    }

    pub fn disallowed_tools(&self) -> &BTreeSet<String> {
        &self.disallowed_tools
    }

    pub fn fork(&self) -> Option<ChildWait> {
        self.fork
    }

    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    pub const fn bare(&self) -> bool {
        self.bare
    }

    pub fn hooks(&self) -> &[HookConfigEntry] {
        &self.hooks
    }

    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }

    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    pub fn render(&self, operation: &SkillOperation) -> Result<String, SkillError> {
        let (name, arguments) = match operation {
            SkillOperation::Invoke {
                name, arguments, ..
            } => (name, arguments),
            SkillOperation::ReadResource { .. } => {
                return Err(SkillError::new(
                    "a resource read cannot render instructions",
                ));
            }
        };
        if name != &self.descriptor.name {
            return Err(SkillError::new("skill operation names a different skill"));
        }
        let body = std::str::from_utf8(&self.body)
            .map_err(|_| SkillError::new("admitted skill body is not UTF-8"))?;
        render_once(body, arguments, &self.arguments)
    }
}

// Interpret only source tokens: inserted argument bytes are never scanned again.
fn render_once(body: &str, arguments: &str, names: &[String]) -> Result<String, SkillError> {
    let mut values: Option<Vec<String>> = None;
    let mut rendered = String::with_capacity(body.len().saturating_add(arguments.len()));
    let mut remaining = body;
    let mut received = false;
    while let Some(offset) = remaining.find('$') {
        let prefix = &remaining[..offset];
        let tail = &remaining[offset + 1..];
        let (length, token) = argument_token(tail, names);
        // Exactly one immediately preceding backslash escapes a known token.
        let escaped = prefix.ends_with('\\') && !prefix.ends_with("\\\\");
        if length > 0 && escaped {
            rendered.push_str(&prefix[..prefix.len() - 1]);
            rendered.push('$');
            rendered.push_str(&tail[..length]);
        } else {
            rendered.push_str(prefix);
            let value = match token {
                Some(ArgumentToken::Raw) => Some(arguments),
                Some(ArgumentToken::Position(index, named)) => {
                    if values.is_none() {
                        values = Some(shell_words::split(arguments).map_err(|error| {
                            SkillError::new(format!("invalid skill argument quoting: {error}"))
                        })?);
                    }
                    let value = index
                        .and_then(|i| values.as_ref()?.get(i))
                        .map(String::as_str);
                    value.or(if named { Some("") } else { None })
                }
                None => None,
            };
            if let Some(value) = value {
                rendered.push_str(value);
                received = true;
            } else {
                rendered.push('$');
                rendered.push_str(&tail[..length]);
            }
        }
        remaining = &tail[length..];
    }
    rendered.push_str(remaining);
    if !received && !arguments.is_empty() {
        rendered.push_str("\n\nARGUMENTS: ");
        rendered.push_str(arguments);
    }
    Ok(rendered)
}

enum ArgumentToken {
    Raw,
    Position(Option<usize>, bool),
}

fn argument_token(tail: &str, names: &[String]) -> (usize, Option<ArgumentToken>) {
    if let Some(indexed) = tail.strip_prefix("ARGUMENTS[") {
        let digits = indexed.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && indexed[digits..].starts_with(']') {
            return (
                11 + digits,
                Some(ArgumentToken::Position(
                    indexed[..digits].parse().ok(),
                    false,
                )),
            );
        }
        // A malformed indexed token must not be consumed as the raw prefix.
        return (0, None);
    }
    let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        return (
            digits,
            Some(ArgumentToken::Position(tail[..digits].parse().ok(), false)),
        );
    }
    let length = tail
        .bytes()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == b'_')
        .count();
    let name = &tail[..length];
    if name == "ARGUMENTS" {
        return (length, Some(ArgumentToken::Raw));
    }
    if let Some(index) = names.iter().position(|candidate| candidate == name) {
        return (length, Some(ArgumentToken::Position(Some(index), true)));
    }
    (0, None)
}

/// One canonical immutable catalogue used by user and model admission.
#[derive(Clone, Debug)]
pub struct SkillCatalogue {
    specs: Arc<[SkillSpec]>,
    evidence: SkillCatalogueEvidence,
    generation: SkillCatalogueGeneration,
}

impl SkillCatalogue {
    pub fn new(mut specs: Vec<SkillSpec>) -> Result<Self, SkillError> {
        if specs
            .iter()
            .any(|spec| !spec.descriptor.user_invocable && !spec.descriptor.model_invocable)
        {
            return Err(SkillError::new(
                "skill is unavailable to both user and model",
            ));
        }
        specs.sort_by(|left, right| left.descriptor.name.cmp(&right.descriptor.name));
        let mut by_name = BTreeSet::new();
        if specs
            .iter()
            .any(|spec| !by_name.insert(spec.descriptor.name.clone()))
        {
            return Err(SkillError::new("duplicate skill name"));
        }
        let evidence =
            SkillCatalogueEvidence::new(specs.iter().map(|spec| spec.descriptor.clone()).collect())
                .map_err(|error| SkillError::new(error.to_string()))?;
        let generation = SkillCatalogueGeneration::derive(&evidence);
        Ok(Self {
            specs: specs.into(),
            evidence,
            generation,
        })
    }

    pub fn evidence(&self) -> &SkillCatalogueEvidence {
        &self.evidence
    }

    pub fn specs(&self) -> &[SkillSpec] {
        &self.specs
    }

    pub fn generation(&self) -> SkillCatalogueGeneration {
        self.generation
    }

    pub fn announce_for_user(&self) -> Vec<&SkillDescriptorEvidence> {
        self.evidence
            .descriptors()
            .iter()
            .filter(|descriptor| descriptor.user_invocable)
            .collect()
    }

    pub fn announce_for_model(&self) -> Vec<&SkillDescriptorEvidence> {
        self.evidence
            .descriptors()
            .iter()
            .filter(|descriptor| descriptor.model_invocable)
            .collect()
    }

    pub fn authorize_user(&self, name: &str) -> Result<&SkillSpec, SkillError> {
        self.authorize(name, |descriptor| descriptor.user_invocable)
    }

    pub fn authorize_user_operation(
        &self,
        operation: &AdmittedSkillOperation,
    ) -> Result<&SkillSpec, SkillError> {
        let spec = self.authorize_user(operation.name())?;
        if spec.descriptor.id != operation.descriptor_id() {
            return Err(SkillError::new(format!(
                "admitted skill operation for {} is stale: descriptor id does not match the current catalogue",
                operation.name()
            )));
        }
        Ok(spec)
    }

    pub fn authorize_model(&self, name: &str) -> Result<&SkillSpec, SkillError> {
        self.authorize(name, |descriptor| descriptor.model_invocable)
    }

    fn authorize(
        &self,
        name: &str,
        visible: impl FnOnce(&SkillDescriptorEvidence) -> bool,
    ) -> Result<&SkillSpec, SkillError> {
        let spec = self
            .specs
            .iter()
            .find(|spec| spec.descriptor.name == name)
            .ok_or_else(|| SkillError::new(format!("there is no admitted skill named {name}")))?;
        if !visible(&spec.descriptor) {
            return Err(SkillError::new(format!(
                "skill {name} is not visible to this caller"
            )));
        }
        Ok(spec)
    }
}

/// Per-turn authority. Every update is subtractive and conflicts fail closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSkillScope {
    effect_tools: BTreeSet<String>,
    model: Option<String>,
    effort: Option<String>,
}

impl ActiveSkillScope {
    pub fn new<I, S>(baseline: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            effect_tools: baseline.into_iter().map(Into::into).collect(),
            model: None,
            effort: None,
        }
    }

    pub fn pin_model(&mut self, model: &str, effort: &str) -> Result<(), SkillError> {
        self.pin_selection(Some(model), Some(effort))
    }

    pub fn pin_selection(
        &mut self,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<(), SkillError> {
        if model.is_some_and(|model| self.model.as_deref().is_some_and(|pinned| pinned != model))
            || effort.is_some_and(|effort| {
                self.effort
                    .as_deref()
                    .is_some_and(|pinned| pinned != effort)
            })
        {
            return Err(SkillError::new(
                "skill model or effort conflicts with the active turn",
            ));
        }
        if let Some(model) = model {
            self.model.get_or_insert_with(|| model.to_owned());
        }
        if let Some(effort) = effort {
            self.effort.get_or_insert_with(|| effort.to_owned());
        }
        Ok(())
    }

    pub fn narrow_effect_tools<I, S>(&mut self, tools: I) -> Result<(), SkillError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let requested: BTreeSet<String> = tools.into_iter().map(Into::into).collect();
        if !requested.is_subset(&self.effect_tools) {
            return Err(SkillError::new(
                "skill tool ceiling would widen active authority",
            ));
        }
        self.effect_tools = requested;
        Ok(())
    }

    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }
    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }
    pub(crate) fn thinking(&self) -> Option<crate::generator::types::ThinkingLevel> {
        self.effort.as_deref().and_then(parse_skill_effort)
    }
    pub fn effect_tools(&self) -> Vec<&str> {
        self.effect_tools.iter().map(String::as_str).collect()
    }
}

/// Body-free, bounded authority snapshot referenced by the instruction fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillInvocationAdmission {
    version: u8,
    invocation: SkillInvocationId,
    descriptor: SkillDescriptorEvidence,
    catalogue_generation: SkillCatalogueGeneration,
    allowed_tools: Option<BTreeSet<String>>,
    disallowed_tools: BTreeSet<String>,
    model: Option<String>,
    effort: Option<String>,
    fork: Option<String>,
    profile: Option<String>,
    /// The declared refusal of the parent's posture, carried in the evidence
    /// because the run it spawns is admitted from this blob and nothing else.
    #[serde(default)]
    bare: bool,
    hooks: Vec<AdmittedSkillHook>,
    resource_paths: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedSkillInvocationAdmission {
    version: u8,
    invocation: SkillInvocationId,
    descriptor: SkillDescriptorEvidence,
    catalogue_generation: SkillCatalogueGeneration,
    allowed_tools: Option<BTreeSet<String>>,
    disallowed_tools: BTreeSet<String>,
    model: Option<String>,
    effort: Option<String>,
    fork: Option<String>,
    profile: Option<String>,
    #[serde(default)]
    bare: bool,
    hooks: Vec<AdmittedSkillHook>,
    resource_paths: Vec<String>,
}

impl<'de> Deserialize<'de> for SkillInvocationAdmission {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let decoded = DecodedSkillInvocationAdmission::deserialize(deserializer)?;
        let admission = Self {
            version: decoded.version,
            invocation: decoded.invocation,
            descriptor: decoded.descriptor,
            catalogue_generation: decoded.catalogue_generation,
            allowed_tools: decoded.allowed_tools,
            disallowed_tools: decoded.disallowed_tools,
            model: decoded.model,
            effort: decoded.effort,
            fork: decoded.fork,
            profile: decoded.profile,
            bare: decoded.bare,
            hooks: decoded.hooks,
            resource_paths: decoded.resource_paths,
        };
        encode_admission(&admission)
            .map(|_| admission)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmittedSkillHook {
    version: u8,
    lifecycle: HookLifecycle,
    matcher: Option<HookMatcher>,
    once: bool,
    handler: HookHandlerConfig,
    evidence: HookDescriptorEvidence,
}

impl SkillInvocationAdmission {
    fn hook_source(&self) -> Result<HookSource, SkillError> {
        if self.version != 1 || self.hooks.iter().any(|hook| hook.version != 1) {
            return Err(SkillError::new(
                "unsupported admitted skill hook snapshot version",
            ));
        }
        let entries = self
            .hooks
            .iter()
            .map(|hook| {
                HookConfigEntry::new(hook.lifecycle, hook.matcher.clone(), hook.handler.clone())
                    .with_once(hook.once)
            })
            .collect::<Vec<_>>();
        let source = HookSource::skill_entries_with_authority(
            self.descriptor.location.as_str(),
            self.invocation,
            self.descriptor.source,
            entries,
        );
        let catalogue =
            HookCatalogue::admit_typed(&[], std::slice::from_ref(&source), Trust::Trusted)
                .map_err(|error| SkillError::new(error.to_string()))?;
        let expected = self
            .hooks
            .iter()
            .map(|hook| {
                HookCatalogueEntryEvidence::new(
                    hook.evidence.clone(),
                    HookActivation::Skill {
                        invocation: self.invocation,
                    },
                )
            })
            .collect::<Vec<_>>();
        if catalogue.evidence().as_slice() != expected.as_slice() {
            return Err(SkillError::new(
                "admitted skill hook snapshot does not match its descriptor evidence",
            ));
        }
        Ok(source)
    }
}

fn encode_admission(admission: &SkillInvocationAdmission) -> Result<Vec<u8>, SkillError> {
    let bytes =
        serde_json::to_vec(admission).map_err(|error| SkillError::new(error.to_string()))?;
    if bytes.len() > MAX_ADMISSION_BYTES {
        return Err(SkillError::new(
            "skill invocation admission exceeds 65536 bytes",
        ));
    }
    Ok(bytes)
}

fn decode_admission(bytes: &[u8]) -> Result<SkillInvocationAdmission, SkillError> {
    if bytes.len() > MAX_ADMISSION_BYTES {
        return Err(SkillError::new(
            "skill invocation admission exceeds 65536 bytes",
        ));
    }
    let admission: SkillInvocationAdmission =
        serde_json::from_slice(bytes).map_err(|error| SkillError::new(error.to_string()))?;
    if !matches!(
        admission.fork.as_deref(),
        None | Some("foreground") | Some("background")
    ) {
        return Err(SkillError::new("admitted skill has an invalid fork mode"));
    }
    Ok(admission)
}

/// One admitted invocation. The exact rendered body and body-free authority
/// snapshot are independent durable blobs referenced by one forced fact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillInvocation {
    id: SkillInvocationId,
    admission: SkillInvocationAdmission,
    admission_ref: Option<BlobRef>,
    rendered_ref: Option<BlobRef>,
    rendered: Arc<str>,
}

#[derive(Clone, Copy)]
struct SkillAdmissionOrigin {
    turn: TurnId,
    item: Option<ItemId>,
    parent: Option<EventId>,
}

impl SkillAdmissionOrigin {
    const fn turn(turn: TurnId) -> Self {
        Self {
            turn,
            item: None,
            parent: None,
        }
    }

    const fn user(turn: TurnId, item: ItemId) -> Self {
        Self {
            turn,
            item: Some(item),
            parent: None,
        }
    }

    const fn model(turn: TurnId, item: ItemId, parent: EventId) -> Self {
        Self {
            turn,
            item: Some(item),
            parent: Some(parent),
        }
    }
}

impl SkillInvocation {
    pub(crate) fn preflight_scope(
        catalogue: &SkillCatalogue,
        operation: &SkillOperation,
        scope: &mut ActiveSkillScope,
    ) -> Result<(), SkillError> {
        let spec = catalogue.authorize_model(operation.name())?;
        Self::preflight_spec_scope(spec, scope)
    }

    pub(crate) fn preflight_user_scope(
        catalogue: &SkillCatalogue,
        operation: &AdmittedSkillOperation,
        scope: &mut ActiveSkillScope,
    ) -> Result<(), SkillError> {
        let spec = catalogue.authorize_user_operation(operation)?;
        Self::preflight_spec_scope(spec, scope)
    }

    fn preflight_spec_scope(
        spec: &SkillSpec,
        scope: &mut ActiveSkillScope,
    ) -> Result<(), SkillError> {
        scope.pin_selection(spec.model(), spec.effort())?;
        if let Some(allowed) = spec.allowed_tools() {
            let narrowed = scope
                .effect_tools
                .intersection(allowed)
                .cloned()
                .collect::<BTreeSet<_>>();
            scope.narrow_effect_tools(narrowed)?;
        }
        scope.effect_tools = scope
            .effect_tools
            .difference(spec.disallowed_tools())
            .cloned()
            .collect();
        Ok(())
    }

    pub fn admit(
        catalogue: &SkillCatalogue,
        operation: SkillOperation,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
    ) -> Result<Self, SkillError> {
        Self::admit_in_turn(catalogue, operation, blobs, journal, TurnId::new())
            .map(|(invocation, _)| invocation)
    }

    pub(crate) fn admit_in_turn(
        catalogue: &SkillCatalogue,
        operation: SkillOperation,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
        turn: TurnId,
    ) -> Result<(Self, EventId), SkillError> {
        let spec = catalogue.authorize_model(operation.name())?.clone();
        Self::admit_authorized_in_turn(
            catalogue.generation,
            spec,
            operation,
            blobs,
            journal,
            SkillAdmissionOrigin::turn(turn),
        )
    }

    pub(crate) fn admit_in_turn_with_origin(
        catalogue: &SkillCatalogue,
        operation: SkillOperation,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
        turn: TurnId,
        item: ItemId,
        parent: EventId,
    ) -> Result<(Self, EventId), SkillError> {
        let spec = catalogue.authorize_model(operation.name())?.clone();
        Self::admit_authorized_in_turn(
            catalogue.generation,
            spec,
            operation,
            blobs,
            journal,
            SkillAdmissionOrigin::model(turn, item, parent),
        )
    }

    pub(crate) fn admit_user_in_turn_with_origin(
        catalogue: &SkillCatalogue,
        operation: AdmittedSkillOperation,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
        turn: TurnId,
        item: ItemId,
    ) -> Result<(Self, EventId), SkillError> {
        let spec = catalogue.authorize_user_operation(&operation)?.clone();
        Self::admit_authorized_in_turn(
            catalogue.generation,
            spec,
            operation.into_operation(),
            blobs,
            journal,
            SkillAdmissionOrigin::user(turn, item),
        )
    }

    fn admit_authorized_in_turn(
        catalogue_generation: SkillCatalogueGeneration,
        spec: SkillSpec,
        operation: SkillOperation,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
        origin: SkillAdmissionOrigin,
    ) -> Result<(Self, EventId), SkillError> {
        let rendered = spec.render(&operation)?;
        let id = SkillInvocationId::new();
        let admission = invocation_admission(id, catalogue_generation, &spec, operation.bare())?;
        let admission_bytes = encode_admission(&admission)?;
        let admission_ref = commit_blob(blobs, &admission_bytes)?;
        let rendered_ref = commit_blob(blobs, rendered.as_bytes())?;
        let admitted = journal
            .append(
                origin.turn,
                origin.item,
                origin.parent,
                EventKind::SkillInstructionAdmitted {
                    invocation: id,
                    skill: spec.descriptor.id,
                    catalogue_generation,
                    source: spec.descriptor.source,
                    location: spec.descriptor.location.clone(),
                    source_digest: spec.descriptor.source_digest,
                    admission: admission_ref.clone(),
                    rendered: rendered_ref.clone(),
                    model: spec.model().map(str::to_owned),
                    effort: spec.effort().map(str::to_owned),
                },
            )
            .map_err(|error| SkillError::new(error.to_string()))?;
        Ok((
            Self {
                id,
                admission,
                admission_ref: Some(admission_ref),
                rendered_ref: Some(rendered_ref),
                rendered: rendered.into(),
            },
            admitted,
        ))
    }

    pub fn admit_for_service(
        spec: SkillSpec,
        operation: SkillOperation,
    ) -> Result<Self, SkillError> {
        let rendered = spec.render(&operation)?;
        let evidence = SkillCatalogueEvidence::new(vec![spec.descriptor.clone()])
            .map_err(|error| SkillError::new(error.to_string()))?;
        let generation = SkillCatalogueGeneration::derive(&evidence);
        let id = SkillInvocationId::new();
        let admission = invocation_admission(id, generation, &spec, operation.bare())?;
        let admission_bytes = encode_admission(&admission)?;
        let admission_ref = synthetic_blob(&admission_bytes)?;
        // `admit_for_service` receives no blob owner; its child-service seam
        // names the already admitted source bytes supplied by that owner.
        let rendered_ref = synthetic_blob(&spec.body)?;
        Ok(Self {
            id,
            admission,
            admission_ref: Some(admission_ref),
            rendered_ref: Some(rendered_ref),
            rendered: rendered.into(),
        })
    }

    pub fn resume(
        invocation: SkillInvocationId,
        _current: &SkillCatalogue,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
    ) -> Result<Self, SkillError> {
        Self::resume_durable(invocation, blobs, journal)
    }

    pub(crate) fn resume_durable(
        invocation: SkillInvocationId,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
    ) -> Result<Self, SkillError> {
        let event = journal
            .inspect(|state| {
                state.skill_events().iter().find_map(|event| match event {
                    crate::state::SkillEvent::Instruction(admitted)
                        if admitted.invocation == invocation =>
                    {
                        Some((
                            admitted.admission.clone(),
                            admitted.rendered.clone(),
                            admitted.skill,
                            admitted.catalogue_generation,
                            admitted.source,
                            admitted.location.clone(),
                            admitted.source_digest,
                            admitted.model.clone(),
                            admitted.effort.clone(),
                        ))
                    }
                    _ => None,
                })
            })
            .map_err(|error| SkillError::new(error.to_string()))?
            .ok_or_else(|| SkillError::new("skill invocation is not present in durable state"))?;
        let bytes = blobs
            .read(&event.0)
            .map_err(|error| SkillError::new(error.to_string()))?;
        let admission = decode_admission(&bytes)?;
        if admission.version != 1
            || admission.invocation != invocation
            || admission.descriptor.id != event.2
            || admission.catalogue_generation != event.3
            || admission.descriptor.source != event.4
            || admission.descriptor.location != event.5
            || admission.descriptor.source_digest != event.6
            || admission.model != event.7
            || admission.effort != event.8
        {
            return Err(SkillError::new(
                "skill admission blob does not match its durable instruction fact",
            ));
        }
        admission.hook_source()?;
        let rendered_bytes = blobs
            .read(&event.1)
            .map_err(|error| SkillError::new(error.to_string()))?;
        let rendered = String::from_utf8(rendered_bytes)
            .map_err(|_| SkillError::new("rendered skill blob is not valid UTF-8"))?;
        Ok(Self {
            id: invocation,
            admission,
            admission_ref: Some(event.0),
            rendered_ref: Some(event.1),
            rendered: rendered.into(),
        })
    }

    pub(crate) fn apply_captured_scope(
        &self,
        scope: &mut ActiveSkillScope,
    ) -> Result<(), SkillError> {
        scope.pin_selection(
            self.admission.model.as_deref(),
            self.admission.effort.as_deref(),
        )?;
        if let Some(allowed) = self.admission.allowed_tools.as_ref() {
            scope.effect_tools = scope.effect_tools.intersection(allowed).cloned().collect();
        }
        scope.effect_tools = scope
            .effect_tools
            .difference(&self.admission.disallowed_tools)
            .cloned()
            .collect();
        Ok(())
    }

    pub fn id(&self) -> SkillInvocationId {
        self.id
    }
    pub fn hook_source(&self) -> Result<HookSource, SkillError> {
        self.admission.hook_source()
    }
    pub fn model(&self) -> Option<&str> {
        self.admission.model.as_deref()
    }
    pub fn name(&self) -> &str {
        &self.admission.descriptor.name
    }
    pub fn effort(&self) -> Option<&str> {
        self.admission.effort.as_deref()
    }
    pub fn fork_wait(&self) -> Option<ChildWait> {
        match self.admission.fork.as_deref() {
            Some("foreground") => Some(ChildWait::Foreground),
            Some("background") => Some(ChildWait::Background),
            _ => None,
        }
    }
    pub fn profile(&self) -> Option<&str> {
        self.admission.profile.as_deref()
    }
    /// Whether this invocation refuses its parent's workspace posture.
    pub const fn bare(&self) -> bool {
        self.admission.bare
    }
    pub fn subagent_invocation(&self) -> SubagentInvocationId {
        SubagentInvocationId::from_uuid(self.id.uuid())
            .expect("a skill invocation UUIDv7 is a subagent invocation UUIDv7")
    }
    pub fn admission_ref(&self) -> Option<&BlobRef> {
        self.admission_ref.as_ref()
    }
    pub fn rendered_ref(&self) -> Option<&BlobRef> {
        self.rendered_ref.as_ref()
    }
    pub(crate) fn rendered_text(&self) -> &str {
        &self.rendered
    }
    pub(crate) fn provenance_tag(&self) -> String {
        format!(
            "source={:?} location={} digest={}",
            self.admission.descriptor.source,
            self.admission.descriptor.location,
            self.admission.descriptor.source_digest,
        )
    }
    pub(crate) fn resource_root(&self) -> std::path::PathBuf {
        let location = std::path::Path::new(self.admission.descriptor.location.as_str());
        location
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default()
    }

    pub(crate) fn source_authority(&self) -> SkillSourceAuthority {
        self.admission.descriptor.source
    }

    pub(crate) fn validate_resource_path(
        &self,
        target: &CanonPath,
        root: &CanonPath,
    ) -> Result<(), String> {
        if self.admission.resource_paths.is_empty()
            || self.admission.resource_paths.iter().any(|pattern| {
                match PathPattern::parse(pattern) {
                    Ok(pattern) => pattern.matches(target.as_path(), root.as_path()),
                    Err(_) => false,
                }
            })
        {
            Ok(())
        } else {
            Err("skill resource is outside its admitted path policy".to_owned())
        }
    }

    pub(crate) fn record_resource(
        &self,
        logical: SkillSourceLocation,
        bytes: &[u8],
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
        turn: TurnId,
    ) -> Result<(), SkillError> {
        let content = commit_blob(blobs, bytes)?;
        journal
            .append(
                turn,
                None,
                None,
                EventKind::SkillResourceAdmitted {
                    invocation: self.id,
                    skill: self.admission.descriptor.id,
                    location: logical,
                    content,
                },
            )
            .map(|_| ())
            .map_err(|error| SkillError::new(error.to_string()))
    }
    pub fn admission_blob(
        &self,
        blobs: &dyn BlobStore,
    ) -> Result<SkillInvocationAdmission, SkillError> {
        let reference = self
            .admission_ref
            .as_ref()
            .ok_or_else(|| SkillError::new("skill admission blob is unavailable"))?;
        let bytes = blobs
            .read(reference)
            .map_err(|error| SkillError::new(error.to_string()))?;
        let admission = decode_admission(&bytes)?;
        admission.hook_source()?;
        Ok(admission)
    }
    pub fn rendered(&self, blobs: &dyn BlobStore) -> Result<Vec<u8>, SkillError> {
        blobs
            .read(
                self.rendered_ref
                    .as_ref()
                    .ok_or_else(|| SkillError::new("rendered skill blob is unavailable"))?,
            )
            .map_err(|error| SkillError::new(error.to_string()))
    }

    /// Read one logical resource through current path resolution and the
    /// already permit-bound guarded facade, then make its exact bytes durable.
    #[allow(clippy::too_many_arguments)]
    pub fn read_resource(
        &self,
        operation: &SkillOperation,
        authority_root: &CanonPath,
        resolver: &dyn PathResolver,
        guarded: &dyn GuardedIo<'_>,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
        turn: TurnId,
    ) -> Result<Vec<u8>, SkillError> {
        let (invocation, logical) = match operation {
            SkillOperation::ReadResource { invocation, path } => (*invocation, path.as_str()),
            SkillOperation::Invoke { .. } => {
                return Err(SkillError::new("invoke is not a resource read"));
            }
        };
        if invocation != self.id {
            return Err(SkillError::new(
                "resource read names a different skill invocation",
            ));
        }
        let logical = SkillSourceLocation::parse(logical)
            .map_err(|error| SkillError::new(error.to_string()))?;
        let canonical_root = resolver
            .canonicalize(authority_root.as_path())
            .map_err(|error| SkillError::new(error.to_string()))?;
        let target = resolver
            .canonicalize(&canonical_root.as_path().join(logical.as_str()))
            .map_err(|error| SkillError::new(error.to_string()))?;
        if !target.as_path().starts_with(canonical_root.as_path()) {
            return Err(SkillError::new(
                "skill resource resolves outside its admitted root",
            ));
        }
        if !self.admission.resource_paths.is_empty()
            && !self.admission.resource_paths.iter().any(|pattern| {
                match PathPattern::parse(pattern) {
                    Ok(pattern) => pattern.matches(target.as_path(), canonical_root.as_path()),
                    Err(_) => false,
                }
            })
        {
            return Err(SkillError::new(
                "skill resource is outside its admitted path policy",
            ));
        }
        let bytes = guarded
            .read(&target)
            .map_err(|error| SkillError::new(error.to_string()))?;
        let content = commit_blob(blobs, &bytes)?;
        journal
            .append(
                turn,
                None,
                None,
                EventKind::SkillResourceAdmitted {
                    invocation: self.id,
                    skill: self.admission.descriptor.id,
                    location: logical,
                    content,
                },
            )
            .map_err(|error| SkillError::new(error.to_string()))?;
        Ok(bytes)
    }
}

fn invocation_admission(
    invocation: SkillInvocationId,
    catalogue_generation: SkillCatalogueGeneration,
    spec: &SkillSpec,
    requested_bare: bool,
) -> Result<SkillInvocationAdmission, SkillError> {
    let hook_source = HookSource::skill_entries_with_authority(
        spec.descriptor.location.as_str(),
        invocation,
        spec.descriptor.source,
        spec.hooks.clone(),
    );
    let hook_catalogue =
        HookCatalogue::admit_typed(&[], std::slice::from_ref(&hook_source), Trust::Trusted)
            .map_err(|error| SkillError::new(error.to_string()))?;
    let hook_evidence = hook_catalogue.evidence();
    let hooks = spec
        .hooks
        .iter()
        .zip(hook_evidence.as_slice())
        .map(|(entry, evidence)| AdmittedSkillHook {
            version: 1,
            lifecycle: entry.lifecycle(),
            matcher: entry.matcher().cloned(),
            once: entry.once(),
            handler: entry.handler().clone(),
            evidence: evidence.descriptor().clone(),
        })
        .collect();
    Ok(SkillInvocationAdmission {
        version: 1,
        invocation,
        descriptor: spec.descriptor.clone(),
        catalogue_generation,
        allowed_tools: spec.allowed_tools.clone(),
        disallowed_tools: spec.disallowed_tools.clone(),
        model: spec.model().map(str::to_owned),
        effort: spec.effort().map(str::to_owned),
        fork: spec.fork.map(|wait| match wait {
            ChildWait::Foreground => "foreground".to_owned(),
            ChildWait::Background => "background".to_owned(),
        }),
        profile: spec.profile.clone(),
        // Either source is enough and neither is a veto: both can only take
        // the parent's posture away.
        bare: spec.bare || requested_bare,
        hooks,
        resource_paths: spec.resource_paths.clone(),
    })
}

fn commit_blob(blobs: &dyn BlobStore, bytes: &[u8]) -> Result<BlobRef, SkillError> {
    let mut writer = blobs
        .begin()
        .map_err(|error| SkillError::new(error.to_string()))?;
    writer
        .append(bytes)
        .map_err(|error| SkillError::new(error.to_string()))?;
    writer
        .commit()
        .map_err(|error| SkillError::new(error.to_string()))
}

fn synthetic_blob(bytes: &[u8]) -> Result<BlobRef, SkillError> {
    let id = crate::store::blob::BlobId::parse(crate::digest::sha256_hex(bytes))
        .map_err(|error| SkillError::new(error.to_string()))?;
    Ok(BlobRef::new(id, bytes.len() as u64))
}

/// Product result of one skill invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillInvocationOutcome {
    Parent(Box<SkillInvocation>),
    Foreground(ChildWaitResult),
    Background(SubagentInvocationId),
}

/// Consumer-owned async seam shared by direct user and model skill entries.
/// The product adapter is the sole owner of child admission, immediate host
/// launch, foreground collection, and background retention/drain.
pub trait SkillInvocationPort: Send + Sync {
    fn invoke<'a>(
        &'a self,
        invocation: SkillInvocation,
        parent_turn: TurnId,
        parent_item: ItemId,
        parent_event: EventId,
        effect_tools: BTreeSet<String>,
    ) -> BoxFuture<'a, Result<SkillInvocationOutcome, SkillError>>;
}

pub fn parse_skill_effort(value: &str) -> Option<crate::generator::types::ThinkingLevel> {
    use crate::generator::types::ThinkingLevel;
    match value {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" | "balanced" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::XHigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}
