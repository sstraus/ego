//! Advisory analysis facts. Nothing in this module grants execution authority.

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use super::{CanonPath, Op, PreparedCall};
use sha2::{Digest as _, Sha256};

/// Ordering is the aggregation rule: high dominates unknown, then bounded, low.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskClass {
    Low,
    Bounded,
    Unknown,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskReason {
    ReadOnly,
    SessionProcessControl,
    ScopedWrite,
    ProcessEffectsUnknown,
    IdentityIncomplete,
    DynamicCode,
    ForeignEffectsUnknown,
    NetworkEffectsUnknown,
    SecretRelease,
    OrchestrationPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RiskAssessment {
    pub class: RiskClass,
    pub reasons: Vec<RiskReason>,
}

pub fn assess(call: &PreparedCall) -> RiskAssessment {
    let mut risk = RiskAssessment {
        class: RiskClass::Low,
        reasons: Vec::new(),
    };
    for op in call.ops() {
        let (class, reason) = match op {
            Op::Read { .. }
            | Op::List { .. }
            | Op::Search { .. }
            | Op::ReadEvidence { .. }
            | Op::SessionProcess {
                action: super::SessionProcessAction::Inspect,
                ..
            } => (RiskClass::Low, RiskReason::ReadOnly),
            Op::SessionProcess {
                action: super::SessionProcessAction::Cancel,
                ..
            } => (RiskClass::Bounded, RiskReason::SessionProcessControl),
            Op::Write { .. } => (RiskClass::Bounded, RiskReason::ScopedWrite),
            Op::Spawn { spec } => {
                if spec.identity().is_none_or(|identity| !identity.complete()) {
                    risk.reasons.push(RiskReason::IdentityIncomplete);
                }
                if spec
                    .identity()
                    .is_some_and(|identity| identity.dynamic_code)
                {
                    risk.reasons.push(RiskReason::DynamicCode);
                }
                (RiskClass::Unknown, RiskReason::ProcessEffectsUnknown)
            }
            Op::Opaque { .. } => (RiskClass::Unknown, RiskReason::ForeignEffectsUnknown),
            Op::Connect { .. } | Op::ProtocolConnect { .. } => {
                (RiskClass::Unknown, RiskReason::NetworkEffectsUnknown)
            }
            Op::SecretEgress { .. } => (RiskClass::High, RiskReason::SecretRelease),
        };
        risk.class = risk.class.max(class);
        risk.reasons.push(reason);
    }
    risk.reasons.sort();
    risk.reasons.dedup();
    risk
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityFailure {
    Unavailable,
    Resolution,
    Read,
    Changed,
    UnsupportedPlatform,
    ScriptUnresolved,
}

/// Host observation, never serialized into a transcript or an analysis document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionIdentity {
    pub target: CanonPath,
    pub files: Vec<ExecutableFileIdentity>,
    pub failures: Vec<IdentityFailure>,
    pub dynamic_code: bool,
}

impl ExecutionIdentity {
    pub fn complete(&self) -> bool {
        self.failures.is_empty()
            && !self.files.is_empty()
            && self.files.iter().all(|file| file.content.is_some())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutableFileIdentity {
    /// The spelling resolved during observation, including a symlink if present.
    pub named: CanonPath,
    pub resolved: CanonPath,
    pub content: Option<[u8; 32]>,
    pub platform: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisOpinion {
    Allow,
    Deny,
    Escalate,
}

impl AnalysisOpinion {
    pub fn reusable(self, complete: bool) -> bool {
        self != Self::Allow || complete
    }
}

/// A local coalescing lease. Implementations must not hold a filesystem lock
/// while the caller generates an opinion.
pub trait AnalysisLease: Send {
    fn lookup(&self) -> Result<Option<AnalysisOpinion>, String>;
    fn insert(&self, opinion: AnalysisOpinion) -> Result<(), String>;
}

/// Consumer-owned advisory persistence boundary; failure means a cache miss.
pub trait PermissionAnalysisCache: Send + Sync {
    fn acquire(&self, key: [u8; 32]) -> BoxFuture<'_, Result<Box<dyn AnalysisLease + '_>, String>>;
}

/// Every captured policy, backend and advisory contract dimension participates.
pub fn cache_key(
    call: &PreparedCall,
    policy: &crate::turn::GenerationPolicyDigest,
    observation: &super::ExecObservation,
    trust: crate::config::Trust,
    judge: &super::judge::JudgeConfig,
) -> [u8; 32] {
    cache_key_with_prompt(
        call,
        policy,
        observation,
        trust,
        judge,
        &super::judge::prompt_revision(),
    )
}

fn cache_key_with_prompt(
    call: &PreparedCall,
    policy: &crate::turn::GenerationPolicyDigest,
    observation: &super::ExecObservation,
    trust: crate::config::Trust,
    judge: &super::judge::JudgeConfig,
    prompt: &[u8],
) -> [u8; 32] {
    let mut digest = Sha256::new();
    let frame = super::prepared_call::frame;
    frame(&mut digest, b"ego:permission-analysis:analysis1:codec1");
    frame(&mut digest, &call.authority_fingerprint());
    frame(
        &mut digest,
        &serde_json::to_vec(&(policy, observation, trust)).expect("closed analysis context"),
    );
    frame(
        &mut digest,
        &serde_json::to_vec(&(
            &judge.identity,
            &judge.request_model,
            judge.max_output_tokens,
        ))
        .expect("closed judge identity"),
    );
    frame(&mut digest, prompt);
    digest.finalize().into()
}

pub fn complete_identity(call: &PreparedCall) -> bool {
    call.ops().iter().all(|op| match op {
        Op::Spawn { spec } => spec.identity().is_some_and(ExecutionIdentity::complete),
        _ => true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_revision_independently_invalidates_an_analysis_key() {
        use crate::config::{Mode, ModelRef, Ruleset, Trust};
        use crate::permit::{BackendIdentity, ExecCaps, ExecObservation};
        let call = PreparedCall::new(
            "read",
            Op::Read {
                path: CanonPath::new("/ws/file"),
            },
        );
        let policy = crate::turn::GenerationPolicyCapture::new(
            CanonPath::new("/ws"),
            Mode::Auto,
            Ruleset::default(),
            None,
        )
        .unwrap()
        .snapshot()
        .digest();
        let observation = ExecObservation::new(
            BackendIdentity::new("test", "1"),
            ExecCaps::READ_SCOPED,
            "11".repeat(32).try_into().unwrap(),
        );
        let judge = super::super::judge::JudgeConfig {
            identity: ModelRef("provider/model".into()),
            request_model: "model".into(),
            max_output_tokens: Some(100),
        };
        let before = cache_key_with_prompt(
            &call,
            &policy,
            &observation,
            Trust::Trusted,
            &judge,
            b"prompt-v1",
        );
        let after = cache_key_with_prompt(
            &call,
            &policy,
            &observation,
            Trust::Trusted,
            &judge,
            b"prompt-v2",
        );
        assert_ne!(before, after);
    }
}
