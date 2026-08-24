//! The tool half of the loop's dependencies: a registry, the policy that
//! authorises a call, and the raw filesystem a grant is exercised against.
//!
//! Kept as three stages rather than one `run(name, args)` future, for the
//! reason `docs/decisions/2026-08-21-the-turn-loop.md` gives for rejecting a
//! `ToolRunner` port: M1 has to append a permission request *between*
//! authorising and executing, and a single future has no such between.

use serde_json::Value;
use std::sync::Arc;

use crate::generator::types::ToolDescriptor as GenToolDescriptor;
use crate::permit::{
    Authorized, CanonPath, Classification, Fs, GuardedEffects, Pending, PolicyEngine, PrepareCtx,
    PreparedCall, ProcessExecutor, Resolution, Tool, ToolError,
};
use crate::store::evidence::EvidenceStore;

/// Everything the loop needs to turn a model's tool call into an effect.
pub struct Tools<'a> {
    registry: &'a [Arc<dyn Tool>],
    policy: &'a PolicyEngine,
    /// The *raw* filesystem. [`GuardedEffects`] binds one permit to the raw
    /// effect ports at construction, so a fresh facade is built per
    /// authorised call; there is no permit to swap into a shared one.
    fs: &'a dyn Fs,
    process: &'a dyn ProcessExecutor,
    evidence: &'a dyn EvidenceStore,
    cwd: CanonPath,
}

impl<'a> Tools<'a> {
    pub fn new(
        registry: &'a [Arc<dyn Tool>],
        policy: &'a PolicyEngine,
        fs: &'a dyn Fs,
        process: &'a dyn ProcessExecutor,
        evidence: &'a dyn EvidenceStore,
        cwd: CanonPath,
    ) -> Self {
        Self {
            registry,
            policy,
            fs,
            process,
            evidence,
            cwd,
        }
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
            .map(|tool| {
                let descriptor = tool.descriptor();
                GenToolDescriptor {
                    name: descriptor.name,
                    description: descriptor.description,
                    parameters: descriptor.schema.to_value(),
                }
            })
            .collect()
    }

    /// Stage one: find the tool and reduce raw arguments to canonical
    /// operations.
    ///
    /// Both failures return prose rather than a typed error on purpose: they
    /// are answers *to the model*, which is going to read them as the tool
    /// result and try again. A caller that could distinguish them would still
    /// do the same thing with both.
    pub fn prepare(&self, name: &str, args: &str) -> Result<(&'a dyn Tool, PreparedCall), String> {
        let tool = self
            .registry
            .iter()
            .find(|tool| tool.descriptor().name == name)
            .ok_or_else(|| format!("there is no tool named {name}"))?;
        let params = serde_json::from_str(args)
            .map_err(|err| format!("{name} was called with arguments that are not JSON: {err}"))?;
        let call = self.prepare_params(tool.as_ref(), params)?;
        Ok((tool.as_ref(), call))
    }

    /// Prepare raw parameters supplied by the human amendment path.
    ///
    /// The tool is fixed to the one that raised the question. Only its
    /// parameters may change, and they cross the same typed preparation and
    /// canonicalisation boundary as model-supplied parameters.
    pub fn prepare_amendment(
        &self,
        tool: &'a dyn Tool,
        params: Value,
    ) -> Result<PreparedCall, String> {
        self.prepare_params(tool, params)
    }

    fn prepare_params(&self, tool: &'a dyn Tool, params: Value) -> Result<PreparedCall, String> {
        let ctx = PrepareCtx {
            cwd: self.cwd.clone(),
            resolver: self.fs,
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
        tool: &'a dyn Tool,
        authorized: Authorized,
    ) -> Result<crate::permit::ToolResult, ToolError> {
        let (call, permit) = authorized.into_parts();
        let guarded = GuardedEffects::new(permit, self.fs, self.process, self.evidence);
        tool.execute(call, &guarded).await
    }
}
