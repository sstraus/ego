//! Admitted skills, their one typed operation, and invocation-scoped authority.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::ModelRef;
use crate::event::domain::EventKind;
use crate::event::hook::HookLifecycle;
use crate::event::id::{ItemId, SubagentInvocationId, TurnId};
use crate::event::skill::{
    SkillCatalogueEvidence, SkillCatalogueGeneration, SkillDescriptorEvidence, SkillId,
    SkillInvocationId, SkillSourceAuthority, SkillSourceLocation,
};
use crate::hook::HookConfigEntry;
use crate::permit::rule::PathPattern;
use crate::permit::{CanonPath, GuardedIo, PathResolver};
use crate::run::RunSelections;
use crate::store::blob::{BlobRef, BlobStore};
use crate::subagent::{ChildRequest, ChildRunService, ChildWait};
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
    fn new(message: impl Into<String>) -> Self {
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
            SkillOperation::Invoke { name, arguments } => (name, arguments),
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
        Ok(render_once(body, arguments))
    }
}

fn render_once(body: &str, arguments: &str) -> String {
    const TOKEN: &str = "$ARGUMENTS";
    let mut rendered = String::with_capacity(body.len().saturating_add(arguments.len()));
    let mut remaining = body;
    while let Some(offset) = remaining.find(TOKEN) {
        rendered.push_str(&remaining[..offset]);
        rendered.push_str(arguments);
        remaining = &remaining[offset + TOKEN.len()..];
    }
    rendered.push_str(remaining);
    rendered
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillInvocationAdmission {
    invocation: SkillInvocationId,
    descriptor: SkillDescriptorEvidence,
    catalogue_generation: SkillCatalogueGeneration,
    allowed_tools: Option<BTreeSet<String>>,
    disallowed_tools: BTreeSet<String>,
    model: Option<String>,
    effort: Option<String>,
    fork: Option<String>,
    profile: Option<String>,
    hook_lifecycles: Vec<HookLifecycle>,
    resource_paths: Vec<String>,
}

/// One admitted invocation. The exact rendered body and body-free authority
/// snapshot are independent durable blobs referenced by one forced fact.
#[derive(Clone, Debug)]
pub struct SkillInvocation {
    id: SkillInvocationId,
    admission: SkillInvocationAdmission,
    admission_ref: Option<BlobRef>,
    rendered_ref: Option<BlobRef>,
    rendered: Arc<str>,
    spec: Option<SkillSpec>,
}

impl SkillInvocation {
    pub(crate) fn preflight_scope(
        catalogue: &SkillCatalogue,
        operation: &SkillOperation,
        scope: &mut ActiveSkillScope,
    ) -> Result<(), SkillError> {
        let spec = catalogue.authorize_model(operation.name())?;
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
    }

    pub(crate) fn admit_in_turn(
        catalogue: &SkillCatalogue,
        operation: SkillOperation,
        blobs: &dyn BlobStore,
        journal: &SharedJournal<'_>,
        turn: TurnId,
    ) -> Result<Self, SkillError> {
        let spec = catalogue.authorize_model(operation.name())?.clone();
        let rendered = spec.render(&operation)?;
        let id = SkillInvocationId::new();
        let admission = invocation_admission(id, catalogue.generation, &spec);
        let admission_bytes =
            serde_json::to_vec(&admission).map_err(|error| SkillError::new(error.to_string()))?;
        if admission_bytes.len() > MAX_ADMISSION_BYTES {
            return Err(SkillError::new(
                "skill invocation admission exceeds 65536 bytes",
            ));
        }
        let admission_ref = commit_blob(blobs, &admission_bytes)?;
        let rendered_ref = commit_blob(blobs, rendered.as_bytes())?;
        journal
            .append(
                turn,
                None,
                None,
                EventKind::SkillInstructionAdmitted {
                    invocation: id,
                    skill: spec.descriptor.id,
                    catalogue_generation: catalogue.generation,
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
        Ok(Self {
            id,
            admission,
            admission_ref: Some(admission_ref),
            rendered_ref: Some(rendered_ref),
            rendered: rendered.into(),
            spec: Some(spec),
        })
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
        let admission = invocation_admission(id, generation, &spec);
        let admission_bytes =
            serde_json::to_vec(&admission).map_err(|error| SkillError::new(error.to_string()))?;
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
            spec: Some(spec),
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
                state
                    .skill_events()
                    .iter()
                    .find_map(|event| match event {
                        crate::state::SkillEvent::Instruction {
                            invocation: found,
                            admission,
                            rendered,
                            ..
                        } if *found == invocation => {
                            Some((admission.clone(), rendered.clone()))
                        }
                        _ => None,
                    })
            })
            .map_err(|error| SkillError::new(error.to_string()))?
            .ok_or_else(|| SkillError::new("skill invocation is not present in durable state"))?;
        let bytes = blobs
            .read(&event.0)
            .map_err(|error| SkillError::new(error.to_string()))?;
        let admission: SkillInvocationAdmission =
            serde_json::from_slice(&bytes).map_err(|error| SkillError::new(error.to_string()))?;
        if admission.invocation != invocation {
            return Err(SkillError::new(
                "skill admission blob names another invocation",
            ));
        }
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
            spec: None,
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
    pub fn model(&self) -> Option<&str> {
        self.admission.model.as_deref()
    }
    pub fn effort(&self) -> Option<&str> {
        self.admission.effort.as_deref()
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
        serde_json::from_slice(&bytes).map_err(|error| SkillError::new(error.to_string()))
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
) -> SkillInvocationAdmission {
    SkillInvocationAdmission {
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
        hook_lifecycles: spec.hooks.iter().map(HookConfigEntry::lifecycle).collect(),
        resource_paths: spec.resource_paths.clone(),
    }
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

/// Result of starting a real child under one skill invocation.
#[derive(Debug)]
pub struct SkillFork {
    wait: ChildWait,
}

impl SkillFork {
    pub fn wait_mode(&self) -> ChildWait {
        self.wait
    }
}

/// Thin orchestration over the completed child owner.
pub struct SkillService {
    child: ChildRunService,
}

impl SkillService {
    pub fn new(child: ChildRunService) -> Self {
        Self { child }
    }

    pub fn fork(
        &self,
        invocation: SkillInvocation,
        wait: ChildWait,
    ) -> Result<SkillFork, SkillError> {
        let spec = invocation
            .spec
            .as_ref()
            .ok_or_else(|| SkillError::new("resumed invocation has no executable child spec"))?;
        let rendered = invocation
            .rendered_ref
            .clone()
            .ok_or_else(|| SkillError::new("skill child has no rendered task blob"))?;
        let mut selections = RunSelections::new(
            spec.model.clone(),
            spec.effort().and_then(parse_skill_effort),
            None,
            None,
            spec.profile.clone(),
        );
        if let Some(profile) = spec.profile() {
            selections = selections.with_profile_name(profile.to_owned());
        }
        let mut request = ChildRequest::new(
            SubagentInvocationId::new(),
            self.child.parent_run(),
            TurnId::new(),
            ItemId::new(),
            rendered,
            selections,
            wait,
        );
        if let Some(tools) = spec.allowed_tools() {
            request = request.with_requested_tools(tools.iter().cloned());
        }
        if let Some(profile) = spec.profile() {
            request = request.with_requested_profile(profile.to_owned());
        }

        let admission = futures::executor::block_on(self.child.start(request))
            .map_err(|error| SkillError::new(error.to_string()))?;
        if wait == ChildWait::Foreground {
            futures::executor::block_on(admission.wait())
                .map_err(|error| SkillError::new(error.to_string()))?;
        }
        Ok(SkillFork { wait })
    }
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
