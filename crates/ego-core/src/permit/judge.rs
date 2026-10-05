//! Content-minimal request and strict response decoder for the auto-mode judge.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    Change, Confinement, EnvPolicy, Escalation, ExecCaps, Op, PreparedCall, RunConfinement,
};
use crate::config::ModelRef;
use crate::generator::port::AiEvent;
use crate::generator::types::{Conversation, GenRequest, Message, Role, StopReason, ThinkingLevel};

pub const JUDGE_RESPONSE_MAX_BYTES: usize = 16 * 1024;
pub const JUDGE_REASON_MAX_BYTES: usize = 1024;

const JUDGE_INSTRUCTIONS: &str = "Decide only from the canonical authority facts in the user JSON. Return exactly one JSON object with verdict allow, deny, or escalate and a string reason. Use escalate when the bounded facts are insufficient.";
const TRUNCATION_MARKER: &str = "…[truncated]";

pub(crate) fn prompt_revision() -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"ego:judge:content-minimal-v2\0");
    digest.update(JUDGE_INSTRUCTIONS.as_bytes());
    digest.finalize().into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeConfig {
    pub identity: ModelRef,
    pub request_model: String,
    pub max_output_tokens: Option<u64>,
}

/// The only facts permitted to cross into the judge request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeInput {
    canonical: String,
}

impl JudgeInput {
    pub fn new(
        call: &PreparedCall,
        confinement: &RunConfinement,
        exec_caps: ExecCaps,
        escalation: &Escalation,
    ) -> Self {
        let canonical = serde_json::to_string(&CanonicalInput {
            call: CanonicalCall::from(call),
            confinement: CanonicalConfinement::from(confinement),
            exec_caps,
            escalation: CanonicalEscalation::from(escalation),
        })
        .expect("the closed judge projection always serializes");
        Self { canonical }
    }

    pub fn request(&self, config: &JudgeConfig) -> GenRequest {
        let messages = Conversation::new(vec![
            Message::text(Role::System, JUDGE_INSTRUCTIONS),
            Message::text(Role::User, self.canonical.clone()),
        ])
        .expect("two text messages are a valid conversation");
        GenRequest {
            cache: None,
            model: config.request_model.clone(),
            messages,
            tools: Vec::new(),
            thinking: ThinkingLevel::Off,
            max_output_tokens: config.max_output_tokens,
            parallel_tool_calls: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JudgeOutcome {
    Allow { reason: String },
    Deny { reason: String },
    Escalate { reason: String },
}

impl JudgeOutcome {
    pub fn reason(&self) -> &str {
        match self {
            Self::Allow { reason } | Self::Deny { reason } | Self::Escalate { reason } => reason,
        }
    }
}

#[derive(Debug, Default)]
pub struct JudgeDecoder {
    response: Vec<u8>,
    terminal: Option<StopReason>,
    uncertain: Option<&'static str>,
}

impl JudgeDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, event: &AiEvent) {
        match event {
            AiEvent::TextDelta { text } if self.uncertain.is_none() => {
                if self.terminal.is_some() {
                    self.uncertain = Some("judge emitted content after its terminal event");
                } else if self.response.len().saturating_add(text.len()) > JUDGE_RESPONSE_MAX_BYTES
                {
                    self.response.clear();
                    self.uncertain = Some("judge response exceeded 16384 bytes");
                } else {
                    self.response.extend_from_slice(text.as_bytes());
                }
            }
            AiEvent::ToolCall { .. } => {
                self.uncertain = Some("judge attempted a tool call");
            }
            AiEvent::Error { .. } => {
                self.uncertain = Some("judge generation failed");
            }
            AiEvent::Stop(reason) => {
                if self.terminal.replace(*reason).is_some() {
                    self.uncertain = Some("judge emitted more than one terminal event");
                }
            }
            AiEvent::ReasoningDelta { .. }
            | AiEvent::ReasoningState(_)
            | AiEvent::Usage(_)
            | AiEvent::ProviderCost { .. }
            | AiEvent::ProviderFailure { .. }
            | AiEvent::DeliveryObserved(_) => {}
            AiEvent::TextDelta { .. } => {}
        }
    }

    pub fn finish(self) -> JudgeOutcome {
        self.finish_valid().unwrap_or_else(escalate)
    }

    /// Only a valid explicit verdict may enter durable advisory storage.
    pub fn finish_valid(self) -> Result<JudgeOutcome, &'static str> {
        if let Some(reason) = self.uncertain {
            return Err(reason);
        }
        if self.terminal != Some(StopReason::Stop) {
            return Err("judge response was incomplete");
        }
        let response = match serde_json::from_slice::<JudgeResponse>(&self.response) {
            Ok(response) => response,
            Err(_) => return Err("judge response was not one strict JSON verdict"),
        };
        let reason = bounded_reason(response.reason);
        Ok(match response.verdict {
            JudgeVerdict::Allow => JudgeOutcome::Allow { reason },
            JudgeVerdict::Deny => JudgeOutcome::Deny { reason },
            JudgeVerdict::Escalate => JudgeOutcome::Escalate { reason },
        })
    }
}

fn escalate(reason: &str) -> JudgeOutcome {
    JudgeOutcome::Escalate {
        reason: bounded_reason(reason.to_owned()),
    }
}

fn bounded_reason(reason: String) -> String {
    if reason.len() <= JUDGE_REASON_MAX_BYTES {
        return reason;
    }
    let target = JUDGE_REASON_MAX_BYTES - TRUNCATION_MARKER.len();
    let mut end = target;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = reason[..end].to_owned();
    bounded.push_str(TRUNCATION_MARKER);
    bounded
}

#[derive(Serialize)]
struct CanonicalInput {
    call: CanonicalCall,
    confinement: CanonicalConfinement,
    exec_caps: ExecCaps,
    escalation: CanonicalEscalation,
}

/// The closed subset of run policy the accepted judge contract discloses.
/// Keeping this projection explicit prevents future RunConfinement fields
/// from crossing the provider boundary automatically.
#[derive(Serialize)]
struct CanonicalConfinement {
    level: Confinement,
    offline: bool,
    workspace: String,
}

impl From<&RunConfinement> for CanonicalConfinement {
    fn from(confinement: &RunConfinement) -> Self {
        Self {
            level: confinement.level(),
            offline: confinement.offline(),
            workspace: confinement.workspace().to_string(),
        }
    }
}

#[derive(Serialize)]
struct CanonicalCall {
    tool: String,
    operations: Vec<CanonicalOp>,
    risk: super::analysis::RiskAssessment,
}

impl From<&PreparedCall> for CanonicalCall {
    fn from(call: &PreparedCall) -> Self {
        Self {
            tool: call.tool().to_owned(),
            operations: call.ops().iter().map(CanonicalOp::from).collect(),
            risk: super::analysis::assess(call),
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum CanonicalOp {
    Read {
        path: String,
    },
    List {
        root: String,
        pattern_bytes: usize,
        pattern_sha256: String,
    },
    Search {
        root: String,
        query_bytes: usize,
        query_sha256: String,
    },
    Write {
        path: String,
        change: CanonicalChange,
    },
    Spawn {
        argv: Vec<CanonicalArgument>,
        cwd: Option<String>,
        timeout_ms: Option<u128>,
        environment: CanonicalEnvironment,
    },
    ReadEvidence {
        reference: String,
        offset: u64,
    },
    Opaque {
        server: String,
        name: String,
        params_digest: String,
        transport: String,
    },
    Connect {
        target: String,
    },
    SecretEgress {
        hook: String,
        target: String,
        sources: Vec<String>,
        header_sinks: Vec<String>,
    },
}

impl From<&Op> for CanonicalOp {
    fn from(op: &Op) -> Self {
        match op {
            Op::Read { path } => Self::Read {
                path: path.to_string(),
            },
            Op::List { root, pattern } => Self::List {
                root: root.to_string(),
                pattern_bytes: pattern.len(),
                pattern_sha256: digest(pattern.as_bytes()),
            },
            Op::Search { root, query } => Self::Search {
                root: root.to_string(),
                query_bytes: query.len(),
                query_sha256: digest(query.as_bytes()),
            },
            Op::Write { path, change } => Self::Write {
                path: path.to_string(),
                change: CanonicalChange::from(change),
            },
            Op::Spawn { spec } => Self::Spawn {
                argv: spec
                    .argv()
                    .iter()
                    .map(|argument| CanonicalArgument {
                        bytes: argument.as_encoded_bytes().len(),
                        sha256: digest(argument.as_encoded_bytes()),
                    })
                    .collect(),
                cwd: spec.cwd().map(ToString::to_string),
                timeout_ms: spec.timeout().map(|timeout| timeout.as_millis()),
                environment: CanonicalEnvironment::from(spec.env_policy()),
            },
            Op::ReadEvidence { request } => Self::ReadEvidence {
                reference: request.reference.to_string(),
                offset: request.offset,
            },
            Op::Opaque {
                tool,
                params_digest,
                transport,
            } => Self::Opaque {
                server: tool.server().to_owned(),
                name: tool.name().to_owned(),
                params_digest: params_digest.as_str().to_owned(),
                transport: transport.to_string(),
            },
            Op::Connect { target, .. } | Op::ProtocolConnect { target } => Self::Connect {
                target: target.to_string(),
            },
            Op::SecretEgress { authority } => Self::SecretEgress {
                hook: authority.hook().to_string(),
                target: authority.target().to_string(),
                sources: authority.sources().to_vec(),
                header_sinks: authority.header_sinks().to_vec(),
            },
        }
    }
}

#[derive(Serialize)]
struct CanonicalArgument {
    bytes: usize,
    sha256: String,
}

#[derive(Serialize)]
#[serde(tag = "shape", rename_all = "snake_case")]
enum CanonicalChange {
    Whole {
        bytes: usize,
        sha256: String,
    },
    Replace {
        old_bytes: usize,
        old_sha256: String,
        new_bytes: usize,
        new_sha256: String,
    },
}

impl From<&Change> for CanonicalChange {
    fn from(change: &Change) -> Self {
        match change {
            Change::Whole(bytes) => Self::Whole {
                bytes: bytes.len(),
                sha256: digest(bytes),
            },
            Change::Replace {
                old_string,
                new_string,
            } => Self::Replace {
                old_bytes: old_string.len(),
                old_sha256: digest(old_string.as_bytes()),
                new_bytes: new_string.len(),
                new_sha256: digest(new_string.as_bytes()),
            },
        }
    }
}

#[derive(Serialize)]
struct CanonicalEnvironment {
    policy: &'static str,
    names: Vec<String>,
}

impl From<&EnvPolicy> for CanonicalEnvironment {
    fn from(policy: &EnvPolicy) -> Self {
        let kind = match policy {
            EnvPolicy::Inherited => "inherited",
            EnvPolicy::AllowOnly(_) => "allow_only",
            EnvPolicy::InheritedExcept(_) => "inherited_except",
            EnvPolicy::Exact { .. } => "exact",
        };
        Self {
            policy: kind,
            names: policy.names().iter().map(ToString::to_string).collect(),
        }
    }
}

#[derive(Serialize)]
struct CanonicalEscalation {
    source: &'static str,
    operation: String,
    path: String,
}

impl From<&Escalation> for CanonicalEscalation {
    fn from(escalation: &Escalation) -> Self {
        Self {
            source: match escalation {
                Escalation::NoRuleCovers { .. } => "rules",
                Escalation::ModeAsks { .. } => "mode",
                Escalation::NoRuleCoversSubject { .. } => "rules",
                Escalation::ModeAsksSubject { .. } => "mode",
            },
            operation: escalation.op().to_string(),
            path: escalation.subject().to_string(),
        }
    }
}

fn digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing into a String cannot fail");
    }
    encoded
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgeResponse {
    verdict: JudgeVerdict,
    reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum JudgeVerdict {
    Allow,
    Deny,
    Escalate,
}
