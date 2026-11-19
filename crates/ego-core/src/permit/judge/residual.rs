//! Residual-write subject of the shared judge: the v3 prompt, the bounded
//! evidence projection and the strict `{grant,path,scope,reason}` decoder.

use serde::{Deserialize, Serialize};

use super::{
    CanonicalCall, CanonicalConfinement, JUDGE_REASON_MAX_BYTES, JudgeConfig, JudgeDecoder,
    JudgeInput, JudgeTaskContext,
};
use crate::config::ModelRef;
use crate::permit::residual::{ResidualScope, ResidualWriteEvidence};
use crate::permit::{ExecCaps, PreparedCall, RunConfinement};

/// The versioned prompt the v3 evaluation set scores.
const RESIDUAL_INSTRUCTIONS: &str =
    include_str!("../../../../../evals/sandbox-auto/v3/judge-system.txt");

#[derive(Serialize)]
struct ResidualFacts<'a> {
    call: CanonicalCall,
    confinement: CanonicalConfinement,
    exec_caps: ExecCaps,
    residual_write: &'a ResidualWriteEvidence,
}

impl JudgeInput {
    /// One refused write candidate of `call`, for the residual-write subject.
    pub fn residual_write(
        call: &PreparedCall,
        confinement: &RunConfinement,
        exec_caps: ExecCaps,
        evidence: &ResidualWriteEvidence,
    ) -> Self {
        let canonical = serde_json::to_string(&ResidualFacts {
            call: CanonicalCall::from(call),
            confinement: CanonicalConfinement::from(confinement),
            exec_caps,
            residual_write: evidence,
        })
        .expect("the closed residual projection always serializes");
        Self::with_facts(RESIDUAL_INSTRUCTIONS, canonical, call)
    }

    /// The candidate exactly as the judge reads it. A write verdict must name
    /// this string; redaction can change it, never widen it.
    pub fn presented_candidate(
        &self,
        config: &JudgeConfig,
        context: &JudgeTaskContext,
        main: Option<&ModelRef>,
    ) -> Option<String> {
        self.facts(config, context, main)["residual_write"]["candidate"]
            .as_str()
            .map(str::to_owned)
    }
}

/// A valid residual verdict. Read and exec verdicts do not exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResidualVerdict {
    Write {
        scope: ResidualScope,
        reason: String,
    },
    None {
        reason: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResidualResponse {
    grant: Grant,
    path: serde_json::Value,
    scope: Scope,
    reason: String,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Grant {
    None,
    Write,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Scope {
    None,
    Session,
    Always,
}

/// Decode one finished generation against the exact presented candidate.
/// Anything but a complete, closed, self-consistent verdict is an error,
/// which the caller treats as no opinion.
pub fn decode_residual(
    decoder: &JudgeDecoder,
    presented: &str,
) -> Result<ResidualVerdict, &'static str> {
    let response = serde_json::from_str::<ResidualResponse>(decoder.finish_text()?)
        .map_err(|_| "residual verdict was not one strict JSON object")?;
    if response.reason.is_empty() || response.reason.len() > JUDGE_REASON_MAX_BYTES {
        return Err("residual verdict reason is empty or longer than 1024 bytes");
    }
    match response.grant {
        Grant::None if response.path.is_null() && response.scope == Scope::None => {
            Ok(ResidualVerdict::None {
                reason: response.reason,
            })
        }
        Grant::Write if response.path.as_str() == Some(presented) => match response.scope {
            Scope::Session => Ok(ResidualVerdict::Write {
                scope: ResidualScope::Session,
                reason: response.reason,
            }),
            Scope::Always => Ok(ResidualVerdict::Write {
                scope: ResidualScope::Always,
                reason: response.reason,
            }),
            Scope::None => Err("residual write verdict has no scope"),
        },
        Grant::Write => Err("residual write verdict names a path other than the candidate"),
        Grant::None => Err("residual none verdict carries a path or scope"),
    }
}

#[cfg(test)]
#[path = "residual_tests.rs"]
mod tests;
