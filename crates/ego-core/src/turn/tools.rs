//! The tool half of the loop's dependencies: a registry and the raw
//! filesystem a policy grant is exercised against.
//!
//! Kept as three stages rather than one `run(name, args)` future, for the
//! reason `ARCHITECTURE.md` gives for rejecting a
//! `ToolRunner` port: M1 has to append a permission request *between*
//! authorising and executing, and a single future has no such between.

use serde_json::Value;

use crate::event::id::ItemId;
use crate::generator::types::ToolDescriptor as GenToolDescriptor;
use crate::mcp::ToolExecution;
use crate::permit::{
    Authorized, CanonPath, Classification, DuplexSpawner, Fs, GuardedEffects, GuardedIo, OneHop,
    Op, Pending, PermissionPresentationInput, PolicyEngine, PrepareCtx, PreparedCall,
    ProcessExecutor, Resolution, Resolver, Tool, ToolError,
};
use crate::store::blob::BlobStore;
use crate::store::evidence::EvidenceStore;
use crate::suspension::{GuardedMcpResume, McpResumeContext, PreparedResume, ResumeSealer};

use super::RegisteredTool;
use super::catalogue::{
    AdmittedToolSource, CapturedToolCatalogue, ToolCaptureAuthority, ToolCatalogueError,
    validate_distinct,
};

struct TaskToolSource {
    capture: CapturedToolCatalogue,
    notes: Vec<String>,
}
impl AdmittedToolSource for TaskToolSource {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        Ok(self.capture.clone())
    }
    fn context_notes(&self) -> Vec<String> {
        self.notes.clone()
    }
}

struct EmptyAdmittedToolSource;

impl AdmittedToolSource for EmptyAdmittedToolSource {
    fn capture(&self) -> Result<CapturedToolCatalogue, ToolCatalogueError> {
        CapturedToolCatalogue::new("static", Vec::new())
    }
}

pub(crate) struct GenerationToolCatalogue {
    generation: std::sync::Arc<str>,
    registry: Vec<RegisteredTool>,
    descriptors: Vec<GenToolDescriptor>,
    authority: ToolCaptureAuthority,
}

impl GenerationToolCatalogue {
    pub(crate) fn generation(&self) -> std::sync::Arc<str> {
        std::sync::Arc::clone(&self.generation)
    }

    pub(crate) fn descriptors(&self) -> Vec<GenToolDescriptor> {
        self.descriptors.clone()
    }

    pub(crate) fn select(&self, name: &str) -> Result<RegisteredTool, String> {
        self.registry
            .iter()
            .find(|registered| registered.matches_name(name))
            .cloned()
            .ok_or_else(|| format!("there is no tool named {name}"))
    }

    pub(crate) fn authority(&self) -> ToolCaptureAuthority {
        self.authority.clone()
    }

    /// What the MCP servers in this generation's catalogue say about
    /// themselves, in ascending server-name order.
    pub(crate) fn instructions(&self) -> &[crate::mcp::McpServerInstructions] {
        self.authority.instructions()
    }

    pub(crate) fn mcp_servers(&self) -> Vec<String> {
        self.authority.server_names()
    }
}

/// A model call after the mixed registry has selected its typed boundary.
/// Human input never acquires an effect tool or enters permission policy.
pub(crate) enum PreparedToolCall {
    Task(super::task::TaskRequest),
    Effect {
        tool: std::sync::Arc<dyn Tool>,
        call: PreparedCall,
        presentation: PermissionPresentationInput,
    },
    HumanInput(crate::question::QuestionRequest),
    History {
        source: std::sync::Arc<dyn crate::history::SessionHistory>,
        request: crate::history::HistoryRequest,
    },
    Skill {
        operation: crate::skill::SkillOperation,
        catalogue: std::sync::Arc<crate::skill::SkillCatalogue>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolBatchError {
    Unknown(String),
    Invalid(String),
    MixedAuthority,
}

/// Raw external-effect ports bound to a tool call only after authorization.
#[derive(Clone)]
pub struct ToolEffectPorts<'a> {
    fs: &'a dyn Fs,
    process: &'a dyn ProcessExecutor,
    evidence: &'a dyn EvidenceStore,
    resolver: std::sync::Arc<dyn Resolver>,
    one_hop: std::sync::Arc<dyn OneHop>,
    duplex: std::sync::Arc<dyn DuplexSpawner>,
    cancel: Option<super::CancelToken>,
    secrets: crate::secrets::SecretValues,
}

impl<'a> ToolEffectPorts<'a> {
    pub fn new(
        fs: &'a dyn Fs,
        process: &'a dyn ProcessExecutor,
        evidence: &'a dyn EvidenceStore,
        resolver: std::sync::Arc<dyn Resolver>,
        one_hop: std::sync::Arc<dyn OneHop>,
        duplex: std::sync::Arc<dyn DuplexSpawner>,
    ) -> Self {
        Self {
            fs,
            process,
            evidence,
            resolver,
            one_hop,
            duplex,
            cancel: None,
            secrets: crate::secrets::SecretValues::default(),
        }
    }

    /// Bind the adapter's known environment values to result publication.
    pub fn with_secret_values(mut self, values: crate::secrets::SecretValues) -> Self {
        self.secrets = values;
        self
    }

    pub fn with_cancel(mut self, cancel: super::CancelToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// Bind one reopened MCP authority to the same guarded effect path used
    /// by an ordinary admitted tool execution.
    pub fn bind_mcp_resume<'session>(
        self,
        authorized: Authorized,
        resume: PreparedResume,
        context: Option<McpResumeContext<'session>>,
    ) -> (PreparedCall, GuardedEffects<'a, GuardedMcpResume<'session>>) {
        let (call, permit) = authorized.into_parts();
        let guarded = GuardedEffects::new_with_resume(
            permit,
            self.fs,
            self.process,
            self.evidence,
            self.resolver,
            self.one_hop,
            self.duplex,
            Some(GuardedMcpResume::new(resume, context)),
        )
        .with_cancel(self.cancel);
        (call, guarded)
    }

    /// Bind one ordinary authorized hook effect to the run's exact raw
    /// effect owners. The permit and prepared call stay paired by consuming
    /// the typed authorization in one step.
    pub fn bind(self, authorized: Authorized) -> (PreparedCall, GuardedEffects<'a>) {
        let (call, permit) = authorized.into_parts();
        let guarded = GuardedEffects::new(
            permit,
            self.fs,
            self.process,
            self.evidence,
            self.resolver,
            self.one_hop,
            self.duplex,
        )
        .with_cancel(self.cancel);
        (call, guarded)
    }

    /// Store complete hook feedback in the same session-bound evidence
    /// authority used by tool output.
    pub fn store_hook_feedback(
        &self,
        bytes: &[u8],
    ) -> Result<crate::store::evidence::EvidenceRef, crate::store::evidence::EvidenceError> {
        let mut writer = self.evidence.begin()?;
        if let Err(error) = writer.append(bytes) {
            let _ = writer.abort();
            return Err(error);
        }
        writer.commit()
    }
}

/// Everything the loop needs to turn a model's tool call into an effect.
pub struct Tools<'a> {
    registry: Vec<RegisteredTool>,
    source: std::sync::Arc<dyn AdmittedToolSource>,
    /// The *raw* filesystem. [`GuardedEffects`] binds one permit to the raw
    /// effect ports at construction, so a fresh facade is built per
    /// authorised call; there is no permit to swap into a shared one.
    effects: ToolEffectPorts<'a>,
    cwd: CanonPath,
    skill_resource_roots: Option<crate::skill::SkillResourceRoots>,
    resume: Option<std::sync::Arc<ResumeSealer>>,
    task_context: Option<(String, std::sync::Arc<futures::lock::Mutex<()>>)>,
}

impl<'a> Tools<'a> {
    pub(crate) fn context_notes(&self) -> Vec<String> {
        self.source.context_notes()
    }

    pub fn new(
        registry: &[std::sync::Arc<dyn Tool>],
        effects: ToolEffectPorts<'a>,
        cwd: CanonPath,
    ) -> Self {
        Self {
            registry: registry
                .iter()
                .cloned()
                .map(RegisteredTool::Effect)
                .collect(),
            source: std::sync::Arc::new(EmptyAdmittedToolSource),
            effects,
            cwd,
            skill_resource_roots: None,
            resume: None,
            task_context: None,
        }
    }

    /// Construct the final mixed registry used when human-input tools are
    /// present alongside effect tools.
    pub fn from_registry(
        registry: &[RegisteredTool],
        effects: ToolEffectPorts<'a>,
        cwd: CanonPath,
    ) -> Self {
        Self {
            registry: registry.to_vec(),
            source: std::sync::Arc::new(EmptyAdmittedToolSource),
            effects,
            cwd,
            skill_resource_roots: None,
            resume: None,
            task_context: None,
        }
    }

    /// Construct static tools plus one owned source captured per generation.
    pub fn from_registry_with_source(
        registry: &[RegisteredTool],
        source: std::sync::Arc<dyn AdmittedToolSource>,
        effects: ToolEffectPorts<'a>,
        cwd: CanonPath,
    ) -> Result<Self, ToolCatalogueError> {
        validate_distinct(registry)?;
        Ok(Self {
            registry: registry.to_vec(),
            source,
            effects,
            cwd,
            skill_resource_roots: None,
            resume: None,
            task_context: None,
        })
    }

    pub fn with_resume(mut self, resume: std::sync::Arc<ResumeSealer>) -> Self {
        self.resume = Some(resume);
        self
    }

    pub fn with_skill_resource_roots(mut self, roots: crate::skill::SkillResourceRoots) -> Self {
        self.skill_resource_roots = Some(roots);
        self
    }

    pub(crate) fn mask_result(&self, result: &mut crate::permit::ToolResult) {
        result.mask_secrets(&self.effects.secrets);
    }

    pub(crate) fn masked_result(
        &self,
        mut result: crate::permit::ToolResult,
    ) -> crate::permit::ToolResult {
        self.mask_result(&mut result);
        result
    }

    pub(crate) fn task_child(
        &self,
        name: String,
        serial: std::sync::Arc<futures::lock::Mutex<()>>,
        cancel: super::CancelToken,
    ) -> Result<Self, ToolCatalogueError> {
        Ok(Self {
            registry: self
                .registry
                .iter()
                .filter(|entry| !matches!(entry, RegisteredTool::Task))
                .cloned()
                .collect(),
            source: std::sync::Arc::new(TaskToolSource {
                capture: self.source.capture()?,
                notes: self.source.context_notes(),
            }),
            effects: self.effects.clone().with_cancel(cancel),
            cwd: self.cwd.clone(),
            skill_resource_roots: self.skill_resource_roots.clone(),
            resume: self.resume.clone(),
            task_context: Some((name, serial)),
        })
    }

    pub(crate) fn blob_store(&self) -> &dyn BlobStore {
        self.effects.evidence.blob_store()
    }

    pub(crate) fn evidence_store(&self) -> &'a dyn EvidenceStore {
        self.effects.evidence
    }

    pub(crate) fn effect_ports(&self) -> ToolEffectPorts<'a> {
        self.effects.clone()
    }

    pub(crate) fn skill_catalogue(&self) -> Option<std::sync::Arc<crate::skill::SkillCatalogue>> {
        self.registry
            .iter()
            .find_map(|registered| match registered {
                RegisteredTool::Skill(catalogue) => Some(std::sync::Arc::clone(catalogue)),
                RegisteredTool::Effect(_)
                | RegisteredTool::HumanInput(_)
                | RegisteredTool::Task
                | RegisteredTool::History { .. } => None,
            })
    }

    pub(crate) fn capture(&self) -> Result<GenerationToolCatalogue, ToolCatalogueError> {
        let dynamic = self.source.capture()?;
        let mut registry = Vec::with_capacity(self.registry.len() + dynamic.tools().len());
        registry.extend(self.registry.iter().cloned());
        registry.extend(dynamic.tools().iter().cloned());
        if self.task_context.is_some() {
            registry.retain(|tool| !matches!(tool, RegisteredTool::Task));
        }
        validate_distinct(&registry)?;
        let descriptors = registry.iter().map(RegisteredTool::descriptor).collect();
        Ok(GenerationToolCatalogue {
            generation: std::sync::Arc::from(dynamic.generation()),
            registry,
            descriptors,
            authority: dynamic.authority(),
        })
    }

    pub(crate) fn capture_with_effect_ceiling(
        &self,
        ceiling: Option<&[&str]>,
    ) -> Result<GenerationToolCatalogue, ToolCatalogueError> {
        let mut captured = self.capture()?;
        if let Some(ceiling) = ceiling {
            captured
                .registry
                .retain(|registered| match registered.as_effect() {
                    Some(tool) => ceiling.contains(&tool.descriptor().name.as_str()),
                    None => true,
                });
            captured.descriptors = captured
                .registry
                .iter()
                .map(RegisteredTool::descriptor)
                .collect();
        }
        Ok(captured)
    }

    pub(crate) fn effect_names(&self) -> Vec<String> {
        let mut names = self
            .registry
            .iter()
            .filter_map(RegisteredTool::as_effect)
            .map(|tool| tool.descriptor().name)
            .collect::<Vec<_>>();
        if let Ok(dynamic) = self.source.capture() {
            names.extend(
                dynamic
                    .tools()
                    .iter()
                    .filter_map(RegisteredTool::as_effect)
                    .map(|tool| tool.descriptor().name),
            );
        }
        names.sort();
        names.dedup();
        names
    }

    pub(crate) fn prepare_skill_resource(
        &self,
        invocation: &crate::skill::SkillInvocation,
        path: &str,
    ) -> Result<
        (
            crate::event::skill::SkillSourceLocation,
            CanonPath,
            PreparedCall,
        ),
        String,
    > {
        let logical = crate::event::skill::SkillSourceLocation::parse(path)
            .map_err(|error| error.to_string())?;
        let relative_root = invocation.resource_root();
        let source_root = self
            .skill_resource_roots
            .as_ref()
            .ok_or_else(|| "skill resource roots are unavailable".to_owned())?
            .root(invocation.source_authority());
        let root = self
            .effects
            .fs
            .canonicalize(&source_root.join(relative_root))
            .map_err(|error| error.to_string())?;
        let target = self
            .effects
            .fs
            .canonicalize(&root.as_path().join(logical.as_str()))
            .map_err(|error| error.to_string())?;
        if !target.as_path().starts_with(root.as_path()) {
            return Err("skill resource resolves outside its admitted root".to_owned());
        }
        invocation.validate_resource_path(&target, &root)?;
        let prepared = PreparedCall::new(
            "skill_resource",
            Op::Read {
                path: target.clone(),
            },
        );
        Ok((logical, target, prepared))
    }

    pub(crate) fn read_skill_resource(
        &self,
        authorized: Authorized,
        expected: &CanonPath,
    ) -> Result<Vec<u8>, String> {
        let (call, permit) = authorized.into_parts();
        if call.ops()
            != [Op::Read {
                path: expected.clone(),
            }]
        {
            return Err("skill resource permit does not match the prepared read".to_owned());
        }
        let guarded = GuardedEffects::new(
            permit,
            self.effects.fs,
            self.effects.process,
            self.effects.evidence,
            std::sync::Arc::clone(&self.effects.resolver),
            std::sync::Arc::clone(&self.effects.one_hop),
            std::sync::Arc::clone(&self.effects.duplex),
        );
        guarded.read(expected).map_err(|error| error.to_string())
    }

    /// What the model is told it can call.
    ///
    /// [`crate::permit::ToolDescriptor`] carries a `schemars::Schema` and
    /// [`GenToolDescriptor`] carries a `serde_json::Value`; the conversion is
    /// here rather than in either type because this is the only seam that
    /// needs both — a tool describes itself once, and a request quotes it.
    pub fn descriptors(&self) -> Vec<GenToolDescriptor> {
        self.registry
            .iter()
            .map(RegisteredTool::descriptor)
            .collect()
    }

    /// Stage one: find the tool and reduce raw arguments to canonical
    /// operations.
    ///
    /// Both failures return prose rather than a typed error on purpose: they
    /// are answers *to the model*, which is going to read them as the tool
    /// result and try again. A caller that could distinguish them would still
    /// do the same thing with both.
    pub fn prepare(
        &self,
        name: &str,
        args: &str,
    ) -> Result<(std::sync::Arc<dyn Tool>, PreparedCall), String> {
        let tool = self
            .registry
            .iter()
            .filter_map(RegisteredTool::as_effect)
            .find(|tool| tool.descriptor().name == name)
            .cloned()
            .ok_or_else(|| format!("there is no tool named {name}"))?;
        let params = serde_json::from_str(args)
            .map_err(|err| format!("{name} was called with arguments that are not JSON: {err}"))?;
        let call = self.prepare_params(tool.as_ref(), params)?;
        Ok((tool, call))
    }

    pub(crate) fn prepare_selected(
        &self,
        registered: RegisteredTool,
        args: &str,
    ) -> Result<PreparedToolCall, String> {
        let descriptor = registered.descriptor();
        let name = descriptor.name;
        let params: Value = serde_json::from_str(args).map_err(|error| {
            format!("{name} was called with arguments that are not JSON: {error}")
        })?;
        match &registered {
            RegisteredTool::Effect(tool) => Ok(PreparedToolCall::Effect {
                tool: std::sync::Arc::clone(tool),
                call: self.prepare_params(tool.as_ref(), params.clone())?,
                presentation: PermissionPresentationInput::new(params, descriptor.parameters),
            }),
            RegisteredTool::Task => super::task::parse(params).map(PreparedToolCall::Task),
            RegisteredTool::History { source, tool } => Ok(PreparedToolCall::History {
                source: std::sync::Arc::clone(source),
                request: tool.parse(params)?,
            }),
            RegisteredTool::HumanInput(tool) => {
                tool.parse(params).map(PreparedToolCall::HumanInput)
            }
            RegisteredTool::Skill(catalogue) => {
                let operation: crate::skill::SkillOperation = serde_json::from_value(params)
                    .map_err(|error| format!("skill was called with invalid arguments: {error}"))?;
                if let crate::skill::SkillOperation::Invoke { name, .. } = &operation {
                    catalogue
                        .authorize_model(name)
                        .map_err(|error| error.to_string())?;
                }
                Ok(PreparedToolCall::Skill {
                    operation,
                    catalogue: std::sync::Arc::clone(catalogue),
                })
            }
        }
    }

    /// Validate one provider batch before any call can cross an effect seam.
    pub fn prepare_batch<I, N, A>(&self, calls: I) -> Result<(), ToolBatchError>
    where
        I: IntoIterator<Item = (N, A)>,
        N: AsRef<str>,
        A: AsRef<str>,
    {
        let mut prepared = Vec::new();
        for (name, args) in calls {
            let registered = self
                .registry
                .iter()
                .find(|registered| registered.matches_name(name.as_ref()))
                .cloned()
                .ok_or_else(|| ToolBatchError::Unknown(name.as_ref().to_owned()))?;
            prepared.push(
                self.prepare_selected(registered, args.as_ref())
                    .map_err(ToolBatchError::Invalid)?,
            );
        }
        let skills = prepared
            .iter()
            .filter(|call| matches!(call, PreparedToolCall::Skill { .. }))
            .count();
        if skills > 0 && prepared.len() != 1 {
            return Err(ToolBatchError::MixedAuthority);
        }
        Ok(())
    }

    /// Prepare raw parameters supplied by the human amendment path.
    ///
    /// The tool is fixed to the one that raised the question. Only its
    /// parameters may change, and they cross the same typed preparation and
    /// canonicalisation boundary as model-supplied parameters.
    pub(crate) fn prepare_amendment(
        &self,
        tool: &dyn Tool,
        params: Value,
    ) -> Result<(PreparedCall, PermissionPresentationInput), String> {
        let schema = serde_json::to_value(tool.descriptor().schema)
            .map_err(|error| format!("tool schema could not be rendered: {error}"))?;
        let call = self.prepare_params(tool, params.clone())?;
        Ok((call, PermissionPresentationInput::new(params, schema)))
    }

    fn prepare_params(&self, tool: &dyn Tool, params: Value) -> Result<PreparedCall, String> {
        let ctx = PrepareCtx {
            cwd: self.cwd.clone(),
            resolver: self.effects.fs,
        };
        tool.prepare(params, &ctx)
            .map_err(|err: ToolError| err.to_string())
            .and_then(|call| {
                call.observe_processes(self.effects.process)
                    .map_err(|error| error.to_string())
            })
    }

    pub(crate) fn revalidate_processes(&self, call: &PreparedCall) -> Result<(), String> {
        for op in call.ops() {
            if let Op::Spawn { spec } = op {
                self.effects
                    .process
                    .revalidate(spec)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    pub(crate) fn refresh_processes(&self, call: PreparedCall) -> Result<PreparedCall, String> {
        call.observe_processes(self.effects.process)
            .map_err(|error| error.to_string())
    }

    /// Stage two: ask policy what this call may do.
    ///
    /// Returns the typed [`Classification`], not prose. It used to flatten to
    /// a `String`, which is the same defect as an `HttpError` flattened one
    /// layer down: the value already names what it refused, and discarding the
    /// type at the seam left the loop unable to tell "policy said no" from
    /// "the tool broke". Those are different outcomes now — see `CLI.md` §4.4
    /// and `ARCHITECTURE.md`.
    ///
    /// Three outcomes rather than two, and the third is the one the loop must
    /// not collapse: a call nobody in this run can answer is not a call that
    /// was refused.
    pub fn authorize(&self, policy: &PolicyEngine, call: PreparedCall) -> Classification {
        policy.classify(call)
    }

    pub(crate) fn authorize_presented(
        &self,
        policy: &PolicyEngine,
        call: PreparedCall,
        presentation: PermissionPresentationInput,
    ) -> Classification {
        policy.classify_presented(call, presentation)
    }

    /// Stage two and a half: put a question the rules did not settle to the
    /// next party in the order.
    ///
    /// A separate stage rather than folded into `authorize`, because the two
    /// differ in the thing this module exists to keep straight: `authorize` is
    /// pure and sync and answers from the run's own configuration; this one
    /// awaits somebody. Collapsing them would put an `await` on the path every
    /// authorised read already takes.
    ///
    /// The loop consults; it does not choose who answers. The approver is
    /// built inside [`PolicyEngine::new`] from the resolved mode, so there is
    /// nothing here to pass and nothing to substitute.
    pub async fn settle(&self, policy: &PolicyEngine, pending: Pending) -> Resolution {
        policy.settle(pending).await
    }

    pub(crate) fn settle_judge(
        &self,
        policy: &PolicyEngine,
        pending: Pending,
        outcome: &crate::permit::judge::JudgeOutcome,
        by: crate::permit::Authority,
    ) -> Resolution {
        policy.settle_judge(pending, outcome, by)
    }

    /// Consult the final human party after the loop has made the request
    /// durable. Keeping this separate from mode settlement is the journal's
    /// crash boundary while a person is deciding.
    pub async fn decide(
        &self,
        policy: &PolicyEngine,
        item: ItemId,
        pending: Pending,
    ) -> Resolution {
        if let Some((name, serial)) = &self.task_context {
            let _guard = serial.lock().await;
            if self
                .effects
                .cancel
                .as_ref()
                .is_some_and(super::CancelToken::is_cancelled)
            {
                return Resolution::Ask(pending);
            }
            policy.decide_named(item, pending, Some(name)).await
        } else {
            policy.decide(item, pending).await
        }
    }

    /// Stage three: run it, under the grant policy issued and through the
    /// facade that checks it.
    ///
    /// The one place an [`Authorized`] is split, and it re-pairs the halves in
    /// the next two lines — the tool receives the call the permit was minted
    /// for and a seam bound to that same permit, and there is no instant in
    /// between where a different call could be substituted.
    pub async fn execute<'session>(
        &self,
        policy: &PolicyEngine,
        tool: &dyn Tool,
        authorized: Authorized,
        context: Option<McpResumeContext<'session>>,
    ) -> Result<ToolExecution, ToolError> {
        let resume = if authorized.call().mcp_envelope().is_some() {
            Some(GuardedMcpResume::deferred(
                self.resume
                    .as_ref()
                    .ok_or(crate::suspension::ContinuationError::InvalidKey)?
                    .defer(&authorized, policy.resume_floor())?,
                context,
            ))
        } else {
            None
        };
        let (call, permit) = authorized.into_parts();
        let guarded = GuardedEffects::new_with_resume(
            permit,
            self.effects.fs,
            self.effects.process,
            self.effects.evidence,
            std::sync::Arc::clone(&self.effects.resolver),
            std::sync::Arc::clone(&self.effects.one_hop),
            std::sync::Arc::clone(&self.effects.duplex),
            resume,
        );
        tool.execute(call, &guarded).await
    }
}
