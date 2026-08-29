//! The tool half of the loop's dependencies: a registry, the policy that
//! authorises a call, and the raw filesystem a grant is exercised against.
//!
//! Kept as three stages rather than one `run(name, args)` future, for the
//! reason `docs/decisions/2026-08-21-the-turn-loop.md` gives for rejecting a
//! `ToolRunner` port: M1 has to append a permission request *between*
//! authorising and executing, and a single future has no such between.

use serde_json::Value;

use crate::generator::types::ToolDescriptor as GenToolDescriptor;
use crate::mcp::ToolExecution;
use crate::permit::{
    Authorized, CanonPath, Classification, DuplexSpawner, Fs, GuardedEffects, OneHop, Pending,
    PolicyEngine, PrepareCtx, PreparedCall, ProcessExecutor, Resolution, Resolver, Tool, ToolError,
};
use crate::store::blob::BlobStore;
use crate::store::evidence::EvidenceStore;
use crate::suspension::ResumeSealer;

use super::RegisteredTool;
use super::catalogue::{
    AdmittedToolSource, CapturedToolCatalogue, ToolCatalogueError, validate_distinct,
};

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
            .find(|registered| registered.descriptor().name == name)
            .cloned()
            .ok_or_else(|| format!("there is no tool named {name}"))
    }
}

/// A model call after the mixed registry has selected its typed boundary.
/// Human input never acquires an effect tool or enters permission policy.
pub(crate) enum PreparedToolCall {
    Effect {
        tool: std::sync::Arc<dyn Tool>,
        call: PreparedCall,
    },
    HumanInput(crate::question::QuestionRequest),
}

/// Raw external-effect ports bound to a tool call only after authorization.
pub struct ToolEffectPorts<'a> {
    fs: &'a dyn Fs,
    process: &'a dyn ProcessExecutor,
    evidence: &'a dyn EvidenceStore,
    resolver: std::sync::Arc<dyn Resolver>,
    one_hop: std::sync::Arc<dyn OneHop>,
    duplex: std::sync::Arc<dyn DuplexSpawner>,
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
        }
    }
}

/// Everything the loop needs to turn a model's tool call into an effect.
pub struct Tools<'a> {
    registry: Vec<RegisteredTool>,
    source: std::sync::Arc<dyn AdmittedToolSource>,
    policy: &'a PolicyEngine,
    /// The *raw* filesystem. [`GuardedEffects`] binds one permit to the raw
    /// effect ports at construction, so a fresh facade is built per
    /// authorised call; there is no permit to swap into a shared one.
    effects: ToolEffectPorts<'a>,
    cwd: CanonPath,
    resume: Option<std::sync::Arc<ResumeSealer>>,
}

impl<'a> Tools<'a> {
    pub fn new(
        registry: &[std::sync::Arc<dyn Tool>],
        policy: &'a PolicyEngine,
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
            policy,
            effects,
            cwd,
            resume: None,
        }
    }

    /// Construct the final mixed registry used when human-input tools are
    /// present alongside effect tools.
    pub fn from_registry(
        registry: &[RegisteredTool],
        policy: &'a PolicyEngine,
        effects: ToolEffectPorts<'a>,
        cwd: CanonPath,
    ) -> Self {
        Self {
            registry: registry.to_vec(),
            source: std::sync::Arc::new(EmptyAdmittedToolSource),
            policy,
            effects,
            cwd,
            resume: None,
        }
    }

    /// Construct static tools plus one owned source captured per generation.
    pub fn from_registry_with_source(
        registry: &[RegisteredTool],
        source: std::sync::Arc<dyn AdmittedToolSource>,
        policy: &'a PolicyEngine,
        effects: ToolEffectPorts<'a>,
        cwd: CanonPath,
    ) -> Result<Self, ToolCatalogueError> {
        validate_distinct(registry)?;
        Ok(Self {
            registry: registry.to_vec(),
            source,
            policy,
            effects,
            cwd,
            resume: None,
        })
    }

    pub fn with_resume(mut self, resume: std::sync::Arc<ResumeSealer>) -> Self {
        self.resume = Some(resume);
        self
    }

    pub(crate) fn blob_store(&self) -> &dyn BlobStore {
        self.effects.evidence.blob_store()
    }

    pub(crate) fn capture(&self) -> Result<GenerationToolCatalogue, ToolCatalogueError> {
        let dynamic = self.source.capture()?;
        let mut registry = Vec::with_capacity(self.registry.len() + dynamic.tools().len());
        registry.extend(self.registry.iter().cloned());
        registry.extend(dynamic.tools().iter().cloned());
        validate_distinct(&registry)?;
        let descriptors = registry.iter().map(RegisteredTool::descriptor).collect();
        Ok(GenerationToolCatalogue {
            generation: std::sync::Arc::from(dynamic.generation()),
            registry,
            descriptors,
        })
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
        let name = registered.descriptor().name;
        let params = serde_json::from_str(args).map_err(|error| {
            format!("{name} was called with arguments that are not JSON: {error}")
        })?;
        match &registered {
            RegisteredTool::Effect(tool) => Ok(PreparedToolCall::Effect {
                tool: std::sync::Arc::clone(tool),
                call: self.prepare_params(tool.as_ref(), params)?,
            }),
            RegisteredTool::HumanInput(tool) => {
                tool.parse(params).map(PreparedToolCall::HumanInput)
            }
        }
    }

    /// Prepare raw parameters supplied by the human amendment path.
    ///
    /// The tool is fixed to the one that raised the question. Only its
    /// parameters may change, and they cross the same typed preparation and
    /// canonicalisation boundary as model-supplied parameters.
    pub fn prepare_amendment(
        &self,
        tool: &dyn Tool,
        params: Value,
    ) -> Result<PreparedCall, String> {
        self.prepare_params(tool, params)
    }

    fn prepare_params(&self, tool: &dyn Tool, params: Value) -> Result<PreparedCall, String> {
        let ctx = PrepareCtx {
            cwd: self.cwd.clone(),
            resolver: self.effects.fs,
        };
        tool.prepare(params, &ctx)
            .map_err(|err: ToolError| err.to_string())
    }

    /// Stage two: ask policy what this call may do.
    ///
    /// Returns the typed [`Classification`], not prose. It used to flatten to
    /// a `String`, which is the same defect as an `HttpError` flattened one
    /// layer down: the value already names what it refused, and discarding the
    /// type at the seam left the loop unable to tell "policy said no" from
    /// "the tool broke". Those are different outcomes now — see `CLI.md` §4.4
    /// and `docs/decisions/2026-08-21-two-levels-of-ending.md`.
    ///
    /// Three outcomes rather than two, and the third is the one the loop must
    /// not collapse: a call nobody in this run can answer is not a call that
    /// was refused.
    pub fn authorize(&self, call: PreparedCall) -> Classification {
        self.policy.classify(call)
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
    pub async fn settle(&self, pending: Pending) -> Resolution {
        self.policy.settle(pending).await
    }

    pub(crate) fn settle_judge(
        &self,
        pending: Pending,
        outcome: &crate::permit::judge::JudgeOutcome,
        by: crate::permit::Authority,
    ) -> Resolution {
        self.policy.settle_judge(pending, outcome, by)
    }

    /// Consult the final human party after the loop has made the request
    /// durable. Keeping this separate from mode settlement is the journal's
    /// crash boundary while a person is deciding.
    pub async fn decide(&self, pending: Pending) -> Resolution {
        self.policy.decide(pending).await
    }

    /// Stage three: run it, under the grant policy issued and through the
    /// facade that checks it.
    ///
    /// The one place an [`Authorized`] is split, and it re-pairs the halves in
    /// the next two lines — the tool receives the call the permit was minted
    /// for and a seam bound to that same permit, and there is no instant in
    /// between where a different call could be substituted.
    pub async fn execute(
        &self,
        tool: &dyn Tool,
        authorized: Authorized,
    ) -> Result<ToolExecution, ToolError> {
        let resume = if authorized.call().mcp_envelope().is_some() {
            Some(
                self.resume
                    .as_ref()
                    .ok_or(crate::suspension::ContinuationError::InvalidKey)?
                    .prepare(&authorized, self.policy.resume_floor())?,
            )
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
